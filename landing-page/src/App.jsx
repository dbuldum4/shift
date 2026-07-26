import { useEffect, useState } from "react";

const statement =
  "shift is a blazingly fast, native, opinionated, robust, and resilient file converter built for macOS";

const directions = [
  { id: "statement", number: "01", name: "statement" },
  { id: "manual", number: "02", name: "manual" },
  { id: "entrance", number: "03", name: "entrance" },
  { id: "roster", number: "04", name: "roster" },
  { id: "rule", number: "05", name: "rule" },
  { id: "quiet", number: "06", name: "quiet" },
];

const githubUrl = "https://github.com/dbuldum4/shift";
const releaseUrl = `${githubUrl}/releases`;

const routes = [
  ["documents", "pdf, word, slides, sheets", "docling · pandoc · markitdown"],
  ["web", "urls and local html", "defuddle"],
  ["media", "audio, video, frames, subtitles", "ffmpeg"],
];

function ExternalLink({ href, children, className = "" }) {
  return (
    <a className={className} href={href} target="_blank" rel="noreferrer">
      {children} ↗
    </a>
  );
}

function Switcher({ selected, onSelect }) {
  return (
    <header className="switcher">
      <span className="switcher-label">shift / directions</span>
      <nav aria-label="design directions">
        {directions.map((direction) => (
          <button
            data-active={selected === direction.id}
            key={direction.id}
            onClick={() => onSelect(direction.id)}
            type="button"
            aria-label={`${direction.number} ${direction.name}`}
          >
            <span>{direction.number}</span>
            <span>{direction.name}</span>
          </button>
        ))}
      </nav>
      <span className="switcher-count">06 studies</span>
    </header>
  );
}

function PageNav({ links = ["formats", "cli"], meta }) {
  return (
    <nav className="page-nav" aria-label="page navigation">
      <a className="wordmark" href="#top">shift</a>
      <div>
        {meta && <span>{meta}</span>}
        {links.map((link) => <a href={`#${link}`} key={link}>{link}</a>)}
        <ExternalLink href={githubUrl}>github</ExternalLink>
      </div>
    </nav>
  );
}

function CopyCommand({ command = "cargo install shift-cli" }) {
  const [copied, setCopied] = useState(false);

  async function copy() {
    try {
      await navigator.clipboard.writeText(command);
      setCopied(true);
    } catch {
      const textarea = document.createElement("textarea");
      textarea.value = command;
      textarea.setAttribute("readonly", "");
      textarea.style.position = "fixed";
      textarea.style.opacity = "0";
      document.body.appendChild(textarea);
      textarea.select();
      setCopied(document.execCommand("copy"));
      textarea.remove();
    }
    window.setTimeout(() => setCopied(false), 1400);
  }

  return (
    <button
      className="copy-line"
      onClick={copy}
      type="button"
      aria-label={copied ? `copied ${command}` : `copy ${command}`}
    >
      <span><b>$</b> {command}</span>
      <span>{copied ? "copied" : "copy"}</span>
    </button>
  );
}

function RouteRows() {
  return (
    <div className="route-rows">
      {routes.map(([title, copy, engines], index) => (
        <div className="route-row" key={title}>
          <span>0{index + 1}</span>
          <strong>{title}</strong>
          <p>{copy}</p>
          <small>{engines}</small>
        </div>
      ))}
    </div>
  );
}

function StatementDirection() {
  return (
    <main className="direction direction-statement" id="top">
      <PageNav />
      <section className="statement-hero">
        <p className="kicker">native conversion for macOS</p>
        <h1>files change.<br />meaning stays.</h1>
        <p className="positioning">{statement}</p>
        <div className="actions">
          <ExternalLink className="primary-link" href={releaseUrl}>download shift</ExternalLink>
          <a className="secondary-link" href="#cli">use the cli ↓</a>
        </div>
      </section>
      <section className="section-block statement-routes" id="formats">
        <header>
          <span>one input</span>
          <h2>every route,<br />explicit.</h2>
        </header>
        <RouteRows />
      </section>
      <section className="section-block cli-section" id="cli">
        <header>
          <span>same engine. another surface.</span>
          <h2>built for the window.<br />ready for the terminal.</h2>
        </header>
        <CopyCommand />
      </section>
    </main>
  );
}

function ManualDirection() {
  return (
    <main className="direction direction-manual" id="top">
      <PageNav links={["overview", "formats", "install"]} />
      <article className="manual-column">
        <section className="manual-intro" id="overview">
          <p className="manual-index">overview / 01</p>
          <h1>{statement}</h1>
          <p>select a file or url. choose an output. shift picks an explicit conversion route and returns a new artifact without modifying the source.</p>
          <p className="inline-links"><ExternalLink href={releaseUrl}>download</ExternalLink> <span>·</span> <a href="#install">install the cli ↓</a></p>
        </section>
        <section className="manual-section" id="formats">
          <h2><span>02</span> formats</h2>
          <p><b>documents</b> pdf, docx, pptx, xlsx, html, markdown, text</p>
          <p><b>media</b> audio, video, frames, subtitle tracks, sequence archives</p>
          <p><b>web</b> public urls and local html</p>
        </section>
        <section className="manual-section" id="install">
          <h2><span>03</span> install</h2>
          <p>download the native macOS app or install the command-line surface.</p>
          <CopyCommand />
        </section>
        <footer className="manual-footer">local first · source preserved · app + cli</footer>
      </article>
    </main>
  );
}

function EntranceDirection() {
  return (
    <main className="direction direction-entrance" id="top">
      <PageNav links={[]} meta="native macOS utility" />
      <section className="entrance-hero">
        <span className="entrance-index">03</span>
        <p className="entrance-kicker">a file is only<br />a temporary shape.</p>
        <h1>shift</h1>
        <div className="entrance-bottom">
          <p>{statement}</p>
          <ExternalLink className="underline-link" href={releaseUrl}>download for macOS</ExternalLink>
        </div>
      </section>
      <section className="entrance-flow" id="formats">
        <span>the route</span>
        <h2>drop it in.<br />take it further.</h2>
        <div className="plain-sequence">
          <p><b>01 / source</b><span>file or url</span></p>
          <p><b>02 / process</b><span>explicit engine</span></p>
          <p><b>03 / artifact</b><span>new output</span></p>
        </div>
      </section>
      <section className="entrance-manifesto" id="cli">
        <p>your source stays yours. the route stays visible. the output stays useful.</p>
        <CopyCommand />
      </section>
    </main>
  );
}

function RosterDirection() {
  const roster = [
    ["document", "docling / pandoc / markitdown", "md · html · txt · pdf"],
    ["web", "defuddle", "md · html"],
    ["audio", "ffmpeg", "mp3 · wav · flac"],
    ["video", "ffmpeg", "mp4 · frames · srt"],
    ["batch", "shared queue", "multi-file"],
  ];

  return (
    <main className="direction direction-roster" id="top">
      <PageNav links={["routes"]} />
      <section className="roster-hero">
        <p className="kicker">native file conversion</p>
        <h1>one quiet place<br />for unruly formats.</h1>
        <p className="positioning">{statement}</p>
        <ExternalLink className="primary-link" href={releaseUrl}>download shift</ExternalLink>
      </section>
      <section className="roster-list" id="routes">
        <div className="roster-head"><span>input class</span><span>route</span><span>output</span></div>
        {roster.map((row) => <div className="roster-row" key={row[0]}>{row.map((cell) => <span key={cell}>{cell}</span>)}</div>)}
      </section>
      <section className="roster-end" id="cli">
        <p>the selected source file is never modified.</p>
        <CopyCommand command="shift-cli report.pdf --to markdown" />
      </section>
    </main>
  );
}

function RuleDirection() {
  return (
    <main className="direction direction-rule" id="top">
      <PageNav links={[]} />
      <article className="rule-column">
        <section className="rule-hero">
          <p className="kicker">local conversion</p>
          <h1>{statement}</h1>
          <div className="rules">
            <p><span>01</span><b>source stays</b><small>the selected file is never modified.</small></p>
            <p><span>02</span><b>route stays visible</b><small>engine and invocation provenance remain inspectable.</small></p>
            <p><span>03</span><b>artifact is new</b><small>download, copy, reveal, or open the result.</small></p>
          </div>
          <ExternalLink className="primary-link" href={releaseUrl}>download shift</ExternalLink>
        </section>
        <section className="rule-surfaces">
          <span>two surfaces. one contract.</span>
          <p>the native app and shift-cli expose the same formats, routes, and conversion behavior.</p>
          <p className="format-sentence">documents, urls, html, images, audio, video, frames, subtitles, archives.</p>
          <CopyCommand />
        </section>
      </article>
    </main>
  );
}

function QuietDirection() {
  return (
    <main className="direction direction-quiet" id="top">
      <PageNav links={[]} />
      <section className="quiet-hero">
        <h1>{statement}</h1>
        <div>
          <ExternalLink className="underline-link" href={releaseUrl}>download shift</ExternalLink>
          <CopyCommand />
        </div>
      </section>
      <section className="quiet-notes">
        <p><b>documents</b><span>docling · pandoc · markitdown</span></p>
        <p><b>web</b><span>defuddle</span></p>
        <p><b>media</b><span>ffmpeg</span></p>
        <small>macOS 13+ · source preserved · native app + cli</small>
      </section>
    </main>
  );
}

const studies = {
  statement: StatementDirection,
  manual: ManualDirection,
  entrance: EntranceDirection,
  roster: RosterDirection,
  rule: RuleDirection,
  quiet: QuietDirection,
};

export function App() {
  const initial = window.location.hash.slice(1);
  const [selected, setSelected] = useState(studies[initial] ? initial : "statement");
  const Study = studies[selected];

  useEffect(() => {
    window.history.replaceState(null, "", `#${selected}`);
    window.scrollTo({ top: 0, behavior: "instant" });
  }, [selected]);

  return (
    <div className={`app-shell selected-${selected}`}>
      <Switcher selected={selected} onSelect={setSelected} />
      <div className="direction-frame" key={selected}>
        <Study />
      </div>
    </div>
  );
}
