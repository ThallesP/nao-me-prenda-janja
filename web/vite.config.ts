import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

// Relative base: the activity is served through Discord's
// <app>.discordsays.com proxy, our own domain serves /share directly —
// relative asset URLs work in both contexts.
export default defineConfig({
  base: "./",
  plugins: [react()],
});
