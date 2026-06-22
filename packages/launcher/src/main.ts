// tacet-launcher — alpha.1 stub.
//
// In alpha.1 this is just a placeholder surface so the monorepo's
// TS workspace exists and `moon run launcher:dev` works end-to-end.
// Real app-launcher UX (read xdg desktop entries, fuzzy filter,
// keyboard-driven selection, Wayland layer-shell surface) lands in
// alpha.2.

import { mount } from './app.js';

const root = document.getElementById('root');
if (!root) throw new Error('#root missing from index.html');
mount(root);
