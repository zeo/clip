//! Big plays reported by the games themselves: CS2 and Dota 2 over Valve's game state integration,
//! League of Legends over Riot's live client data api. Nothing is read from the game's memory.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::Value;

/// Where the valve games post their state, on loopback only.
pub const PORT: u16 = 41877;
const CFG: &str = "gamestate_integration_clip.cfg";
const LEAGUE: &str = "https://127.0.0.1:2999/liveclientdata";
// a dota multi-kill is a kill within this long of the one before
const STREAK: Duration = Duration::from_secs(18);

pub struct Game {
    pub id: &'static str,
    pub name: &'static str,
    /// the lowercase exe a highlight has to come from the recording of
    pub exe: &'static str,
    /// the steam app and where in its folder the cfg goes, for the valve games
    valve: Option<(u32, &'static str, &'static [&'static str])>,
    pub plays: &'static str,
}

pub const GAMES: [Game; 3] = [
    Game {
        id: "cs2",
        name: "Counter-Strike 2",
        exe: "cs2.exe",
        valve: Some((730, "game/csgo/cfg", &["round", "player_id", "player_state"])),
        plays: "3 or more kills in a round, and aces",
    },
    Game {
        id: "dota2",
        name: "Dota 2",
        exe: "dota2.exe",
        valve: Some((570, "game/dota/cfg/gamestate_integration", &["player"])),
        plays: "Triple kills, ultra kills and rampages",
    },
    Game {
        id: "league",
        name: "League of Legends",
        exe: "league of legends.exe",
        valve: None,
        plays: "Multi-kills, first blood, aces, and baron, dragon and herald steals",
    },
];

pub fn game(id: &str) -> Option<&'static Game> {
    GAMES.iter().find(|game| game.id == id)
}

#[derive(Serialize)]
pub struct Found {
    id: &'static str,
    name: &'static str,
    plays: &'static str,
    /// why it can't be switched on, or null
    missing: Option<String>,
}

impl Game {
    /// The cfg file the game reads at launch, `None` for league, which needs none.
    pub fn cfg(&self) -> Option<Result<PathBuf, String>> {
        let (appid, dir, _) = self.valve?;
        Some(
            crate::art::install(appid)
                .map(|install| install.join(dir).join(CFG))
                .ok_or_else(|| format!("{} isn't installed through Steam", self.name)),
        )
    }

    /// Writes the cfg that has the game post to clip, or takes it away.
    pub fn connect(&self, on: bool, token: &str) -> Result<(), String> {
        match (on, self.cfg()) {
            (_, None) | (false, Some(Err(_))) => Ok(()),
            (true, Some(cfg)) => self.place(&cfg?, Some(token)),
            (false, Some(Ok(cfg))) => self.place(&cfg, None),
        }
    }

    /// Writes the cfg at `cfg` carrying `token`, or takes it away for none.
    fn place(&self, cfg: &Path, token: Option<&str>) -> Result<(), String> {
        let failed = |e: std::io::Error| format!("can't change {}'s game state file: {e}", self.name);
        match token {
            Some(token) => {
                std::fs::create_dir_all(cfg.parent().expect("a cfg has a folder")).map_err(failed)?;
                std::fs::write(cfg, self.text(token)).map_err(failed)
            }
            None if cfg.exists() => std::fs::remove_file(cfg).map_err(failed),
            None => Ok(()),
        }
    }

    fn text(&self, token: &str) -> String {
        let blocks = ["provider", "map"].iter().chain(self.valve.map_or(&[][..], |(_, _, blocks)| blocks));
        let data: String = blocks.map(|block| format!("\t\t\"{block}\"\t\"1\"\n")).collect();
        format!(
            "\"clip\"\n{{\n\t\"uri\"\t\"http://127.0.0.1:{PORT}/{}\"\n\t\"timeout\"\t\"1.0\"\n\t\"buffer\"\t\"0.1\"\n\t\"throttle\"\t\"0.1\"\n\t\"heartbeat\"\t\"30.0\"\n\t\"auth\"\n\t{{\n\t\t\"token\"\t\"{token}\"\n\t}}\n\t\"data\"\n\t{{\n{data}\t}}\n}}\n",
            self.id
        )
    }

    pub fn found(&self, listening: Option<&str>) -> Found {
        let missing = match self.cfg() {
            Some(Err(e)) => Some(e),
            Some(Ok(_)) => listening.map(|e| format!("clip can't listen for it: {e}")),
            None => None,
        };
        Found { id: self.id, name: self.name, plays: self.plays, missing }
    }
}

/// A token the cfg carries, so only a game clip wrote it for is heard.
pub fn token() -> String {
    use windows::Win32::Security::Cryptography::{BCryptGenRandom, BCRYPT_USE_SYSTEM_PREFERRED_RNG};
    let mut bytes = [0u8; 16];
    unsafe { BCryptGenRandom(None, &mut bytes, BCRYPT_USE_SYSTEM_PREFERRED_RNG) }
        .ok()
        .expect("windows has a random source");
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Remembers what each game said last, to tell when a post is a new highlight.
#[derive(Default)]
pub struct Plays {
    round_kills: Option<u64>,
    kills: Option<u64>,
    streak: Vec<Instant>,
}

impl Plays {
    pub fn cs2(&mut self, post: &Value) -> Option<String> {
        // while dead the player block is whoever is being watched
        if post["player"]["steamid"] != post["provider"]["steamid"] {
            return None;
        }
        let kills = post["player"]["state"]["round_kills"].as_u64()?;
        let was = self.round_kills.replace(kills)?;
        (kills >= 3 && kills > was).then(|| if kills >= 5 { "ace".into() } else { format!("{kills} kills") })
    }

    pub fn dota(&mut self, post: &Value, now: Instant) -> Option<String> {
        let kills = post["player"]["kills"].as_u64()?;
        let was = self.kills.replace(kills)?;
        if kills <= was {
            return None;
        }
        if self.streak.last().is_some_and(|&last| now - last > STREAK) {
            self.streak.clear();
        }
        self.streak.extend(std::iter::repeat_n(now, (kills - was) as usize));
        match self.streak.len() {
            0..3 => None,
            3 => Some("triple kill".into()),
            4 => Some("ultra kill".into()),
            _ => Some("rampage".into()),
        }
    }
}

/// The loopback port the valve games post to.
pub fn bind() -> std::io::Result<TcpListener> {
    TcpListener::bind((Ipv4Addr::LOCALHOST, PORT))
}

/// Hears the valve games' posts, handing each highlight to `send`.
pub fn listen(
    listener: TcpListener,
    token: impl Fn() -> String + Send + 'static,
    send: impl Fn(&'static Game, String) + Send + 'static,
) {
    std::thread::spawn(move || {
        let mut plays = Plays::default();
        for stream in listener.incoming().flatten() {
            if let Some((game, post)) = read(stream, &token()) {
                let play = match game.id {
                    "cs2" => plays.cs2(&post),
                    _ => plays.dota(&post, Instant::now()),
                };
                if let Some(play) = play {
                    send(game, play);
                }
            }
        }
    });
}

/// One post carrying `token`, answered so the game sends the next, and the game its path names.
/// The whole request has two seconds, so a stalled one can't hold the posts behind it.
fn read(stream: TcpStream, token: &str) -> Option<(&'static Game, Value)> {
    if !stream.peer_addr().ok()?.ip().is_loopback() {
        return None;
    }
    let until = Instant::now() + Duration::from_secs(2);
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut next = |line: &mut String| -> Option<()> {
        line.clear();
        stream
            .set_read_timeout(Some(until.checked_duration_since(Instant::now()).filter(|left| !left.is_zero())?))
            .ok()?;
        (&mut reader).take(8 << 10).read_line(line).ok()?;
        line.ends_with('\n').then_some(())
    };
    let mut line = String::new();
    next(&mut line)?;
    let mut words = line.split_whitespace();
    let game = (words.next() == Some("POST")).then(|| words.next()).flatten();
    let game = game.and_then(|path| GAMES.iter().find(|g| g.valve.is_some() && path == format!("/{}", g.id)));
    let mut length = 0;
    loop {
        next(&mut line)?;
        if line.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.trim().eq_ignore_ascii_case("content-length") {
                length = value.trim().parse().ok()?;
            }
        }
    }
    // a whole cs2 post with every block is well under this
    if length > 1 << 20 {
        return None;
    }
    let mut body = vec![0; length];
    let mut got = 0;
    while got < length {
        let left = until.checked_duration_since(Instant::now()).filter(|left| !left.is_zero())?;
        reader.get_ref().set_read_timeout(Some(left)).ok()?;
        match reader.read(&mut body[got..]).ok()? {
            0 => return None,
            read => got += read,
        }
    }
    let post: Option<Value> = serde_json::from_slice(&body).ok();
    let heard = post.as_ref().is_some_and(|post| !token.is_empty() && post["auth"]["token"] == token);
    let answer = match (game, heard) {
        (None, _) => "404 Not Found",
        (_, false) => "401 Unauthorized",
        _ => "200 OK",
    };
    let mut stream = reader.into_inner();
    let _ = write!(stream, "HTTP/1.1 {answer}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    Some((game?, post.filter(|_| heard)?))
}

/// Asks league for new events every second while `playing` says it's the game being recorded.
pub fn watch_league(
    playing: impl Fn() -> bool + Send + 'static,
    send: impl Fn(&'static Game, String) + Send + 'static,
) {
    std::thread::spawn(move || {
        let game = game("league").expect("league is listed");
        let mut seen = None;
        loop {
            std::thread::sleep(Duration::from_secs(1));
            let read = || -> Option<(Value, Value)> {
                let me =
                    serde_json::from_slice(&crate::art::get(&format!("{LEAGUE}/activeplayername"), None).ok()?).ok()?;
                let events =
                    serde_json::from_slice(&crate::art::get(&format!("{LEAGUE}/eventdata"), None).ok()?).ok()?;
                Some((me, events))
            };
            match playing().then(read).flatten() {
                Some((Value::String(me), events)) => {
                    for play in league(&mut seen, &me, &events) {
                        send(game, play);
                    }
                }
                _ => seen = None,
            }
        }
    });
}

/// The highlights among events newer than `seen`. The first look at a match only takes note of
/// where it is, since what came before isn't in the buffer.
fn league(seen: &mut Option<u64>, me: &str, events: &Value) -> Vec<String> {
    let events = events["Events"].as_array().map_or(&[][..], Vec::as_slice);
    let id = |event: &Value| event["EventID"].as_u64();
    let last = events.iter().filter_map(id).max();
    // ids start over with each match
    let from = std::mem::replace(seen, last.or(*seen)).filter(|&from| last.is_none_or(|last| last >= from));
    let Some(from) = from else { return Vec::new() };
    // events name players by their riot id's game name, the active player comes with the tag
    let same = |name: &Value| {
        let plain = |name: &str| name.split('#').next().unwrap_or("").trim().to_lowercase();
        name.as_str().is_some_and(|name| plain(name) == plain(me))
    };
    // riot has sent these as strings and as their own types
    let stolen = |event: &Value| {
        event["Stolen"]
            .as_bool()
            .unwrap_or_else(|| event["Stolen"].as_str().is_some_and(|s| s.eq_ignore_ascii_case("true")))
    };
    let streak = |event: &Value| event["KillStreak"].as_u64().or_else(|| event["KillStreak"].as_str()?.parse().ok());
    events
        .iter()
        .filter(|event| id(event).is_some_and(|id| id > from))
        .filter_map(|event| {
            let play = match event["EventName"].as_str()? {
                "Multikill" if same(&event["KillerName"]) => match streak(event)? {
                    0..2 => return None,
                    2 => "double kill",
                    3 => "triple kill",
                    4 => "quadra kill",
                    _ => "penta kill",
                },
                "FirstBlood" if same(&event["Recipient"]) => "first blood",
                "Ace" if same(&event["Acer"]) => "ace",
                "BaronKill" if stolen(event) && same(&event["KillerName"]) => "baron steal",
                "DragonKill" if stolen(event) && same(&event["KillerName"]) => "dragon steal",
                "HeraldKill" if stolen(event) && same(&event["KillerName"]) => "herald steal",
                _ => return None,
            };
            Some(play.to_string())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cs2(steamid: &str, kills: u64) -> Value {
        json!({"provider": {"steamid": "76561"}, "player": {"steamid": steamid, "state": {"round_kills": kills}}})
    }

    #[test]
    fn cs2_counts_the_players_own_kills_from_three_up() {
        let mut plays = Plays::default();
        let said: Vec<_> = [0, 1, 2, 3, 3, 4, 0, 5].iter().map(|&kills| plays.cs2(&cs2("76561", kills))).collect();
        assert_eq!(
            said,
            [None, None, None, Some("3 kills".into()), None, Some("4 kills".into()), None, Some("ace".into())]
        );
        // dead and watching a teammate with 4 kills
        assert_eq!(plays.cs2(&cs2("76599", 4)), None);
        // clip started mid round, after the kills
        assert_eq!(Plays::default().cs2(&cs2("76561", 3)), None);
    }

    #[test]
    fn dota_names_kills_close_together() {
        let mut plays = Plays::default();
        let start = Instant::now();
        let at = |s: u64| start + Duration::from_secs(s);
        let post = |kills: u64| json!({"player": {"kills": kills}});
        assert_eq!(plays.dota(&post(0), at(0)), None);
        assert_eq!(plays.dota(&post(1), at(1)), None);
        assert_eq!(plays.dota(&post(2), at(10)), None);
        assert_eq!(plays.dota(&post(3), at(20)), Some("triple kill".into()));
        // a kill after the streak ran out starts again
        assert_eq!(plays.dota(&post(4), at(60)), None);
        assert_eq!(plays.dota(&post(7), at(61)), Some("ultra kill".into()));
        assert_eq!(plays.dota(&post(8), at(62)), Some("rampage".into()));
    }

    #[test]
    fn league_takes_the_active_players_new_plays_once() {
        let events = |list: Value| json!({ "Events": list });
        let early = events(json!([
            {"EventID": 0, "EventName": "GameStart"},
            {"EventID": 1, "EventName": "FirstBlood", "Recipient": "Tux"},
        ]));
        let later = events(json!([
            {"EventID": 0, "EventName": "GameStart"},
            {"EventID": 1, "EventName": "FirstBlood", "Recipient": "Tux"},
            {"EventID": 2, "EventName": "Multikill", "KillerName": "Tux", "KillStreak": 3},
            {"EventID": 3, "EventName": "Multikill", "KillerName": "Other", "KillStreak": 2},
            {"EventID": 4, "EventName": "BaronKill", "KillerName": "tux", "Stolen": "True"},
            {"EventID": 5, "EventName": "DragonKill", "KillerName": "Tux", "Stolen": "False"},
        ]));
        let mut seen = None;
        assert!(league(&mut seen, "Tux#EUW", &early).is_empty());
        assert_eq!(league(&mut seen, "Tux#EUW", &later), ["triple kill", "baron steal"]);
        assert!(league(&mut seen, "Tux#EUW", &later).is_empty());
        // a new match counts from its own start
        let next = events(
            json!([{"EventID": 0, "EventName": "GameStart"}, {"EventID": 1, "EventName": "FirstBlood", "Recipient": "Tux"}]),
        );
        assert!(league(&mut seen, "Tux#EUW", &next).is_empty());
        assert_eq!(seen, Some(1));
    }

    // shaped after valve's and riot's documented payloads, with the blocks the cfg asks for
    const CS2: &str = include_str!("../tests/fixtures/cs2.json");
    const DOTA: &str = include_str!("../tests/fixtures/dota2.json");
    const LEAGUE: &str = include_str!("../tests/fixtures/league.json");

    #[test]
    fn whole_posts_from_each_game_give_their_plays() {
        let cs2: Value = serde_json::from_str(CS2).unwrap();
        let mut plays = Plays::default();
        let mut at = |kills: u64| {
            let mut post = cs2.clone();
            post["player"]["state"]["round_kills"] = kills.into();
            plays.cs2(&post)
        };
        assert_eq!((at(2), at(3)), (None, Some("3 kills".into())));
        let dota: Value = serde_json::from_str(DOTA).unwrap();
        let mut plays = Plays::default();
        let now = Instant::now();
        let mut at = |kills: u64| {
            let mut post = dota.clone();
            post["player"]["kills"] = kills.into();
            plays.dota(&post, now)
        };
        assert_eq!((at(4), at(7)), (None, Some("triple kill".into())));
        let league_events: Value = serde_json::from_str(LEAGUE).unwrap();
        let mut seen = Some(2);
        assert_eq!(league(&mut seen, "Tux#EUW", &league_events), ["double kill", "dragon steal", "triple kill"]);
    }

    #[test]
    fn a_cfg_is_written_with_its_folder_and_taken_away() {
        let root = std::env::temp_dir().join(format!("clip-cfg-{}", std::process::id()));
        let cfg = root.join("game/csgo/cfg").join(CFG);
        GAMES[0].place(&cfg, Some("abc")).unwrap();
        assert!(std::fs::read_to_string(&cfg).unwrap().contains("\"token\"\t\"abc\""));
        GAMES[0].place(&cfg, None).unwrap();
        assert!(!cfg.exists());
        assert!(GAMES[0].place(&cfg, None).is_ok(), "a cfg that's gone already is fine");
        let _ = std::fs::remove_dir_all(&root);
        assert!(GAMES[2].connect(true, "abc").is_ok(), "league needs no file");
    }

    #[test]
    fn the_cfg_follows_valves_format() {
        let text = GAMES[0].text("abc");
        assert!(text.starts_with("\"clip\"\n{\n\t\"uri\"\t\"http://127.0.0.1:41877/cs2\"\n"), "{text}");
        assert!(text.contains("\t\"auth\"\n\t{\n\t\t\"token\"\t\"abc\"\n\t}\n"), "{text}");
        assert!(
            text.contains("\t\t\"round\"\t\"1\"\n\t\t\"player_id\"\t\"1\"\n\t\t\"player_state\"\t\"1\"\n\t}\n}\n"),
            "{text}"
        );
        assert!(GAMES[1].text("abc").contains("\t\t\"map\"\t\"1\"\n\t\t\"player\"\t\"1\"\n"));
        assert_eq!(token().len(), 32);
        assert_ne!(token(), token());
    }

    #[test]
    fn the_listener_hears_only_its_token_on_its_path() {
        // the real port may be held by a running clip
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let at = listener.local_addr().unwrap();
        let post = |path: &str, body: &str| {
            let mut stream = TcpStream::connect(at).unwrap();
            write!(stream, "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: {}\r\n\r\n{body}", body.len())
                .unwrap();
            let mut answer = String::new();
            stream.read_to_string(&mut answer).unwrap();
            answer.split_whitespace().nth(1).unwrap_or_default().to_string()
        };
        let (tx, rx) = std::sync::mpsc::channel();
        listen(listener, || "right".into(), move |game, play| tx.send((game.id, play)).unwrap());
        let body = |token: &str, kills: u64| {
            json!({"auth": {"token": token}, "provider": {"steamid": "1"}, "player": {"steamid": "1", "state": {"round_kills": kills}}})
                .to_string()
        };
        assert_eq!(post("/cs2", &body("right", 2)), "200");
        assert_eq!(post("/cs2", &body("wrong", 3)), "401");
        assert_eq!(post("/league", &body("right", 3)), "404");
        assert_eq!(post("/dota2", &body("right", 3)), "200");
        // a stalled request is dropped once its time is up, and the next is heard
        let mut stalled = TcpStream::connect(at).unwrap();
        write!(stalled, "POST /cs2 HTTP/1.1\r\nContent-Length: 500\r\n\r\n{{").unwrap();
        let mut long = TcpStream::connect(at).unwrap();
        write!(long, "POST /cs2 HTTP/1.1\r\nX: {}", "a".repeat(9000)).unwrap();
        assert_eq!(post("/cs2", &body("right", 3)), "200");
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), ("cs2", "3 kills".into()));
        assert!(rx.try_recv().is_err());
    }
}
