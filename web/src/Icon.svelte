<script lang="ts">
  import { convertFileSrc } from '@tauri-apps/api/core';

  let { app, icon, size = 28 }: { app: string; icon: string | null; size?: number } = $props();
</script>

<span class="app-icon" style:--size="{size}px">
  {#if icon}<img src={convertFileSrc(icon)} alt="" draggable="false" />{:else}<span class="proper">{app.slice(0, 1).toUpperCase()}</span>{/if}
</span>
