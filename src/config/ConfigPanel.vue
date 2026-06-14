<script setup lang="ts">
// Config slice view. PR2 turns this into the full editable config form.
import { onMounted } from "vue";
import { useConfig } from "./useConfig";

const { config, load } = useConfig();
onMounted(load);
</script>

<template>
  <section class="config-panel">
    <h2>Config</h2>
    <p v-if="config" class="muted">Monitoring: {{ config.repo }}</p>
  </section>
</template>

<style scoped>
.config-panel { margin-bottom: 16px; }
.muted { color: #888; }
</style>
