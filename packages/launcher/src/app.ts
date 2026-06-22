export function mount(host: HTMLElement): void {
  host.innerHTML = `
    <main class="launcher">
      <header>
        <h1>tacet-launcher</h1>
        <p class="status">v0.1.0-alpha.1 · placeholder surface</p>
      </header>
      <section>
        <p>
          The real launcher (xdg desktop-entry scanner + fuzzy filter +
          layer-shell surface) ships in alpha.2.
        </p>
        <p>
          For now this confirms the TS workspace is wired up:
          <code>moon run launcher:dev</code> serves this file at
          <code>http://127.0.0.1:20100</code>.
        </p>
      </section>
    </main>
  `;
}
