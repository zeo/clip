<script lang="ts">
  import { invoke } from './host';
  import { chord, keyName } from './types';

  // plain takes a single key with no modifiers, for push to talk
  let { mods = $bindable(0), key = $bindable(0), plain = false, label }: { mods?: number; key: number; plain?: boolean; label: string } = $props();
  let listening = $state(false);
  const shown = $derived(listening ? 'Press a key' : plain ? (key ? keyName(key) : 'None') : chord({ mods, key }));

  // clip's own hotkeys are let go while listening, so pressing one reaches this box instead of saving a clip
  function hold(on: boolean) {
    listening = on;
    invoke('hold_hotkeys', { on });
  }

  function press(e: KeyboardEvent & { currentTarget: HTMLButtonElement }) {
    if (e.key === 'Tab') return;
    e.preventDefault();
    e.stopPropagation();
    if (['Control', 'Shift', 'Alt', 'Meta'].includes(e.key)) return;
    const held = (e.altKey ? 1 : 0) | (e.ctrlKey ? 2 : 0) | (e.shiftKey ? 4 : 0) | (e.metaKey ? 8 : 0);
    if (e.key === 'Backspace' && !held) [key, mods] = [0, 0];
    else if (e.key !== 'Escape') [key, mods] = [e.keyCode, plain ? 0 : held];
    e.currentTarget.blur();
  }
</script>

<button class="field bind mono" class:listening aria-label="{shown}, {label}" title="Press a key, Backspace clears" onfocus={() => hold(true)} onblur={() => hold(false)} onkeydown={press}>
  {shown}
</button>
