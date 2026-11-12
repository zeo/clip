<script lang="ts">
  import { onMount } from 'svelte';
  import { invoke } from './host';
  import Bind from './Bind.svelte';
  import { ACTIONS, bufferCost, does, length, type Mic, type Settings } from './types';

  let { ondone }: { ondone: () => void } = $props();
  const buffers = [15, 30, 60, 120, 300, 600, 1800, 3600, 7200];
  const steps = ['Keys', 'Sound', 'Clips'];
  let at = $state(0);
  let draft = $state<Settings | null>(null);
  let saved: Settings | null = null;
  let mics = $state<Mic[]>([]);
  let error = $state('');
  let busy = $state(false);
  let box = $state<HTMLDialogElement>();
  // the first key for each of these, the rest stay on the hotkeys page
  const keys = $derived(draft ? ['save', 'mark'].flatMap((action) => draft!.hotkeys.filter((h) => h.action === action).slice(0, 1)) : []);

  onMount(async () => {
    box?.showModal();
    try {
      const s = await invoke<Settings>('settings');
      saved = structuredClone(s);
      draft = s;
      mics = await invoke<Mic[]>('mics');
    } catch (e) {
      error = String(e);
    }
  });

  async function pickFolder() {
    const picked = await invoke<string | null>('pick_folder', { from: draft!.folder });
    if (picked) draft!.folder = picked;
  }

  // skipping applies what clip started with, which writes the settings file so the tour stays away next time
  async function finish(settings: Settings | null) {
    if (!settings) return;
    busy = true;
    error = '';
    try {
      await invoke('apply', { settings });
      box?.close();
      ondone();
    } catch (e) {
      error = String(e);
    } finally {
      busy = false;
    }
  }
</script>

<dialog class="sheet tour" bind:this={box} aria-labelledby="tour-title" onkeydown={(e) => e.stopPropagation()} oncancel={(e) => (e.preventDefault(), finish(saved))}>
  <div class="sheet-body">
    <header class="sheet-head">
      <h2 id="tour-title">{['Two keys to know', 'What clips hear', 'Where clips go'][at]}</h2>
      <span class="mono mute">{at + 1} of {steps.length}</span>
    </header>
    {#if draft}
      {#if at === 0}
        <p class="form-hint">clip records the game in focus all the time and keeps the last {length(draft.buffer)}. These keys work inside games. Click a box, then press the keys.</p>
        <div class="rows">
          {#each keys as hotkey (hotkey.action)}
            <div class="row">
              <span class="row-label">{ACTIONS[hotkey.action].name}<small>{does(hotkey.action, draft.buffer)}</small></span>
              <Bind bind:mods={hotkey.mods} bind:key={hotkey.key} label={ACTIONS[hotkey.action].name} />
            </div>
          {/each}
        </div>
      {:else if at === 1}
        <p class="form-hint">Game audio, Discord and everything else each get a track of their own, so one can be turned down later.</p>
        <div class="rows">
          <label class="row">
            <span class="row-label">Record the microphone<small>{mics.length ? 'On a track of its own too' : 'No microphone is plugged in'}</small></span>
            <input type="checkbox" bind:checked={draft.mic} />
          </label>
          <label class="row">
            <span class="row-label">Microphone device</span>
            <select class="field" bind:value={draft.micDevice} disabled={!draft.mic}>
              <option value="">Default device</option>
              {#each mics as mic (mic.id)}<option value={mic.id}>{mic.name}</option>{/each}
            </select>
          </label>
        </div>
      {:else}
        <div class="rows">
          <div class="row">
            <span class="row-label">Clips folder<small class="row-path mono">{draft.folder}</small></span>
            <button class="gel" onclick={pickFolder}>Change</button>
          </div>
          <label class="row">
            <span class="row-label">Keep the last<small>{bufferCost(draft)} at the current video settings</small></span>
            <select class="field" bind:value={draft.buffer}>
              {#each buffers as b (b)}<option value={b}>{length(b)}</option>{/each}
            </select>
          </label>
        </div>
        <p class="form-hint tour-note">Everything else is in Settings, and <kbd class="key">?</kbd> lists the keys.</p>
      {/if}
    {/if}
    {#if error}<p class="form-error" role="alert">{error}</p>{/if}
    <footer class="tour-foot">
      <button class="gel" disabled={busy} onclick={() => finish(saved)}>Skip</button>
      <span class="grow"></span>
      {#if at > 0}<button class="gel" onclick={() => at--}>Back</button>{/if}
      {#if at < steps.length - 1}
        <button class="gel default" disabled={!draft} onclick={() => at++}>Next</button>
      {:else}
        <button class="gel default" disabled={busy || !draft} onclick={() => finish(draft)}>Start clipping</button>
      {/if}
    </footer>
  </div>
</dialog>
