const statement =
  "shift is a blazingly fast, native, opinionated, and robust file converter built for macOS";

const githubUrl = "https://github.com/dbuldum4/shift";
const releaseUrl = `${githubUrl}/releases/tag/v1.2.0`;

function ExternalLink({ href, children, className = "" }) {
  return (
    <a className={className} href={href} target="_blank" rel="noreferrer">
      {children} ↗
    </a>
  );
}

export function App() {
  return (
    <main id="top">
      <nav className="page-nav" aria-label="page navigation">
        <a className="wordmark" href="#top">shift</a>
        <div>
          <ExternalLink href={`${githubUrl}/releases`}>releases</ExternalLink>
          <ExternalLink href={githubUrl}>github</ExternalLink>
        </div>
      </nav>

      <section className="hero" aria-labelledby="intro">
        <h1 id="intro">{statement}</h1>
        <ExternalLink className="download-link" href={releaseUrl}>download shift 1.2.0</ExternalLink>
      </section>

      <footer>
        <div className="routes" aria-label="conversion engines">
          <p><b>documents</b><span>docling · pandoc · markitdown</span></p>
          <p><b>spreadsheets</b><span>calamine · csv · xlsxwriter</span></p>
          <p><b>web</b><span>defuddle</span></p>
          <p><b>images</b><span>sips · native ImageIO</span></p>
          <p><b>media</b><span>ffmpeg</span></p>
        </div>
        <small>macOS 13+ · source preserved · native app + cli · not yet Developer ID signed or notarized</small>
      </footer>
    </main>
  );
}
