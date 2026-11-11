<script lang="ts">
  import { fileSrc } from './host';

  let { app, icon, size = 28 }: { app: string; icon: string | null; size?: number } = $props();
</script>

<span class="app-icon" class:has={icon} style:--size="{size}px">
  {#if icon}<img src={fileSrc(icon)} alt="" draggable="false" />{:else}{app.slice(0, 1).toUpperCase()}{/if}
</span>
