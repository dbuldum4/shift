import { useEffect, useState } from "react";
import {
  ArrowDown,
  ArrowRight,
  ArrowUpRight,
  Check,
  Copy,
  FileArrowDown,
  FilePdf,
  FileText,
  GithubLogo,
  Lightning,
  MusicNotes,
  Play,
  TerminalWindow,
} from "@phosphor-icons/react";

const directions = [
  { id: "terminal", number: "01", name: "Terminal" },
  { id: "workbench", number: "02", name: "Workbench" },
  { id: "signal", number: "03", name: "Signal" },
  { id: "receipt", number: "04", name: "Receipt" },
  { id: "blueprint", number: "05", name: "Blueprint" },
  { id: "noir", number: "06", name: "Noir" },
];

const formats = ["PDF", "DOCX", "PPTX", "XLSX", "HTML", "MP4", "MP3", "PNG"];
const githubUrl = "https://github.com/dbuldum4/shift";
const releaseUrl = `${githubUrl}/releases`;

function ExternalLink({ href, children, className = "" }) {
  return (
    <a className={className} href={href} target="_blank" rel="noreferrer">
      {children}
      <ArrowUpRight aria-hidden="true" weight="bold" />
    </a>
  );
}

function DirectionSwitcher({ selected, onSelect }) {
  return (
    <header className="direction-bar">
      <a className="direction-brand" href="#top" aria-label="Shift landing page directions">
        <span className="brand-mark">s/</span>
        <span>shift directions</span>
      </a>
      <nav aria-label="Design directions">
        {directions.map((direction) => (
          <button
            className="direction-tab"
            data-active={selected === direction.id}
            key={direction.id}
            onClick={() => onSelect(direction.id)}
            type="button"
          >
            <span>{direction.number}</span>
            <span>{direction.name}</span>
          </button>
        ))}
      </nav>
      <span className="direction-count">06 studies</span>
    </header>
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
      className="copy-command"
      onClick={copy}
      type="button"
      aria-label={copied ? `Copied ${command}` : `Copy ${command}`}
    >
      <span><b>$</b> {command}</span>
      {copied ? <Check aria-hidden="true" weight="bold" /> : <Copy aria-hidden="true" />}
    </button>
  );
}

function TerminalCard() {
  return (
    <div className="terminal-card" aria-label="Shift command line conversion example">
      <div className="terminal-top">
        <span>shift-cli</span>
        <span>conversion / 01</span>
      </div>
      <div className="terminal-line">
        <span className="prompt">$</span>
        <span>shift-cli quarterly-report.pdf --to markdown</span>
      </div>
      <div className="terminal-progress">
        <span>reading structure</span><span>done</span>
        <span>extracting tables</span><span>done</span>
        <span>writing artifact</span><span className="cyan">100%</span>
      </div>
      <div className="artifact-row">
        <FileText aria-hidden="true" weight="duotone" />
        <span>quarterly-report.md</span>
        <span>84.2 KB</span>
      </div>
    </div>
  );
}

function ConverterWindow({ light = false }) {
  return (
    <div className={`converter-window ${light ? "is-light" : ""}`}>
      <div className="window-chrome">
        <span className="traffic"><i /><i /><i /></span>
        <span>Shift</span>
        <span>⌘ /</span>
      </div>
      <div className="window-content">
        <div className="drop-column">
          <span className="eyebrow">source</span>
          <div className="drop-file">
            <FilePdf aria-hidden="true" weight="duotone" />
            <strong>field-notes.pdf</strong>
            <span>24 pages · 9.6 MB</span>
          </div>
          <div className="engine-row"><span>engine</span><b>Docling</b></div>
        </div>
        <div className="conversion-arrow"><ArrowRight aria-hidden="true" /></div>
        <div className="result-column">
          <span className="eyebrow">artifact</span>
          <div className="document-lines">
            <b># Field notes</b>
            <i /><i /><i className="short" />
            <b>## Observations</b>
            <i /><i className="short" />
          </div>
          <div className="ready-row"><span><i /> ready</span><b>Markdown</b></div>
        </div>
      </div>
    </div>
  );
}

function PipelineStrip() {
  return (
    <div className="pipeline-strip" aria-label="Shift conversion pipeline">
      <div><span>01</span><b>Select</b><small>file or URL</small></div>
      <ArrowRight aria-hidden="true" />
      <div><span>02</span><b>Convert</b><small>best engine</small></div>
      <ArrowRight aria-hidden="true" />
      <div><span>03</span><b>Download</b><small>your artifact</small></div>
    </div>
  );
}

function TerminalDirection() {
  return (
    <main className="study terminal-study" id="top">
      <nav className="study-nav">
        <a className="wordmark" href="#top">shift</a>
        <div>
          <a href="#capabilities">formats</a>
          <a href="#cli">cli</a>
          <ExternalLink href={githubUrl}>github</ExternalLink>
        </div>
      </nav>
      <section className="terminal-hero">
        <div className="hero-copy">
          <p className="kicker">native conversion for macOS</p>
          <h1>Files change.<br />Meaning stays.</h1>
          <p className="lede">Shift converts documents, media, and web pages into clean, downloadable artifacts—without touching the source.</p>
          <div className="hero-actions">
            <ExternalLink className="text-cta" href={releaseUrl}>Download Shift</ExternalLink>
            <a className="muted-cta" href="#cli">Use the CLI <ArrowDown aria-hidden="true" /></a>
          </div>
        </div>
        <TerminalCard />
      </section>
      <section className="terminal-capabilities" id="capabilities">
        <div className="section-heading">
          <span>one input</span>
          <h2>Every route,<br />explicit.</h2>
        </div>
        <div className="capability-list">
          {[
            ["Documents", "PDF, Word, slides, sheets", "MarkItDown · Pandoc · Docling"],
            ["Web", "URLs and local HTML", "Defuddle"],
            ["Media", "Audio, video, frames, subtitles", "FFmpeg"],
          ].map(([title, copy, engines], index) => (
            <article key={title}>
              <span>0{index + 1}</span>
              <h3>{title}</h3>
              <p>{copy}</p>
              <small>{engines}</small>
            </article>
          ))}
        </div>
      </section>
      <section className="terminal-cli" id="cli">
        <div>
          <p className="kicker">same engine. another surface.</p>
          <h2>Built for the window.<br />Ready for the terminal.</h2>
        </div>
        <CopyCommand />
      </section>
    </main>
  );
}

function WorkbenchDirection() {
  return (
    <main className="study workbench-study" id="top">
      <nav className="study-nav">
        <a className="wordmark" href="#top">shift</a>
        <div>
          <a href="#workflow">workflow</a>
          <a href="#engines">engines</a>
          <ExternalLink href={githubUrl}>source</ExternalLink>
        </div>
      </nav>
      <section className="workbench-hero">
        <div className="workbench-title">
          <p className="kicker">file conversion, considered</p>
          <h1>Make the file<br />fit the work.</h1>
        </div>
        <div className="workbench-note">
          <span>01 / 06</span>
          <p>A focused native utility for reshaping documents, media, and web pages.</p>
          <ExternalLink className="pill-cta" href={releaseUrl}>Get Shift for macOS</ExternalLink>
        </div>
        <ConverterWindow light />
      </section>
      <section className="workbench-flow" id="workflow">
        <p className="section-index">02 / workflow</p>
        <PipelineStrip />
      </section>
      <section className="format-field" id="engines">
        <div>
          <p className="kicker">wide in. precise out.</p>
          <h2>One quiet place<br />for unruly formats.</h2>
        </div>
        <div className="format-cloud">
          {formats.map((format) => <span key={format}>{format}</span>)}
        </div>
      </section>
    </main>
  );
}

function SignalDirection() {
  return (
    <main className="study signal-study" id="top">
      <nav className="study-nav">
        <a className="signal-wordmark" href="#top"><span>sh</span><span>ift</span></a>
        <div><span>macOS 13+</span><ExternalLink href={githubUrl}>github</ExternalLink></div>
      </nav>
      <section className="signal-hero">
        <div className="signal-label"><Lightning aria-hidden="true" weight="fill" /> universal artifact shifter</div>
        <h1><span>in:</span> anything.<br /><span>out:</span> exactly<br />what you need.</h1>
        <div className="signal-aside">
          <p>Native file conversion with visible engines, honest provenance, and zero source mutation.</p>
          <ExternalLink className="signal-button" href={releaseUrl}>Download .dmg</ExternalLink>
        </div>
      </section>
      <section className="signal-marquee" aria-label="Supported formats">
        <div>{formats.map((format) => <span key={format}>{format} <b>→</b></span>)}</div>
      </section>
      <section className="signal-grid">
        <article>
          <TerminalWindow aria-hidden="true" weight="duotone" />
          <span>01</span>
          <h2>One conversion contract.</h2>
          <p>The native app and shift-cli run the same capability-aware pipeline.</p>
        </article>
        <article className="signal-card-accent">
          <FileArrowDown aria-hidden="true" weight="duotone" />
          <span>02</span>
          <h2>Source stays untouched.</h2>
          <p>Every conversion returns a new, downloadable artifact.</p>
        </article>
        <article>
          <Play aria-hidden="true" weight="duotone" />
          <span>03</span>
          <h2>Media is first-class.</h2>
          <p>Trim, encode, extract frames, subtitles, and audio with FFmpeg.</p>
        </article>
      </section>
    </main>
  );
}

function ReceiptDirection() {
  return (
    <main className="study receipt-study" id="top">
      <nav className="receipt-nav">
        <a className="receipt-logo" href="#top">SHIFT®</a>
        <span>ARTIFACT CONVERTER / MACOS</span>
        <ExternalLink href={githubUrl}>SOURCE</ExternalLink>
      </nav>
      <section className="receipt-hero">
        <div className="receipt-stamp">BUILD 0.1<br />LOCAL FIRST</div>
        <p className="receipt-overline">CONVERSION DOES NOT HAVE TO BE A CLOUD SERVICE.</p>
        <h1>SELECT.<br />SHIFT.<br />SAVE.</h1>
        <div className="receipt-intro">
          <p>Convert documents, URLs, images, audio, and video through a native macOS interface or the command line.</p>
          <ExternalLink className="receipt-cta" href={releaseUrl}>[ DOWNLOAD LATEST ]</ExternalLink>
        </div>
      </section>
      <section className="receipt-table">
        <div className="receipt-row receipt-head"><span>INPUT CLASS</span><span>ROUTE</span><span>OUTPUT</span><span>STATUS</span></div>
        {[
          ["DOCUMENT", "DOCLING / PANDOC", "MD · HTML · PDF", "READY"],
          ["WEB PAGE", "DEFUDDLE", "MD · HTML", "READY"],
          ["AUDIO", "FFMPEG", "MP3 · WAV · FLAC", "READY"],
          ["VIDEO", "FFMPEG", "MP4 · FRAMES · SRT", "READY"],
        ].map((row) => <div className="receipt-row" key={row[0]}>{row.map((cell) => <span key={cell}>{cell}</span>)}</div>)}
      </section>
      <section className="receipt-bottom">
        <div><span>CLI / QUICK START</span><CopyCommand command="shift-cli report.pdf --to markdown" /></div>
        <div><span>RULE / 001</span><p>THE SELECTED SOURCE FILE IS NEVER MODIFIED.</p></div>
      </section>
    </main>
  );
}

function BlueprintDirection() {
  return (
    <main className="study blueprint-study" id="top">
      <nav className="study-nav">
        <a className="blueprint-logo" href="#top">SHIFT<span>/01</span></a>
        <div>
          <a href="#system">system</a>
          <a href="#routes">routes</a>
          <ExternalLink href={githubUrl}>repository</ExternalLink>
        </div>
      </nav>
      <section className="blueprint-hero" id="system">
        <div className="blueprint-copy">
          <p className="kicker">conversion system / macOS</p>
          <h1>Reshape the<br />artifact.<br /><em>Keep the signal.</em></h1>
          <p>Shift dispatches every source through an explicit, inspectable conversion route—then hands the finished artifact back to you.</p>
          <ExternalLink className="blueprint-cta" href={releaseUrl}>Run Shift <ArrowRight aria-hidden="true" /></ExternalLink>
        </div>
        <div className="blueprint-map">
          <div className="map-node map-source"><span>source</span><b>report.pdf</b><small>9.6 MB</small></div>
          <div className="map-line line-one"><i /></div>
          <div className="map-node map-engine"><span>module</span><b>Docling</b><small>layout aware</small></div>
          <div className="map-line line-two"><i /></div>
          <div className="map-node map-output"><span>artifact</span><b>report.md</b><small>84.2 KB</small></div>
          <span className="map-coordinate c1">X 0142</span>
          <span className="map-coordinate c2">Y 0084</span>
        </div>
      </section>
      <section className="blueprint-routes" id="routes">
        {[
          ["A", "documents", "structure preserved"],
          ["B", "web", "clutter removed"],
          ["C", "media", "streams controlled"],
          ["D", "batch", "queue shared"],
        ].map(([code, title, copy]) => <article key={code}><span>{code}</span><h2>{title}</h2><p>{copy}</p></article>)}
      </section>
    </main>
  );
}

function NoirDirection() {
  return (
    <main className="study noir-study" id="top">
      <nav className="study-nav">
        <a className="wordmark" href="#top">shift</a>
        <div><span>native macOS utility</span><ExternalLink href={githubUrl}>github</ExternalLink></div>
      </nav>
      <section className="noir-hero">
        <div className="noir-number">06</div>
        <p className="noir-kicker">A file is only<br />a temporary shape.</p>
        <h1>SHI<span>F</span>T</h1>
        <div className="noir-footer">
          <p>Documents. Web. Media.<br />One local conversion surface.</p>
          <ExternalLink className="noir-cta" href={releaseUrl}>Download for macOS</ExternalLink>
        </div>
      </section>
      <section className="noir-stage">
        <div className="noir-stage-copy">
          <span>the route</span>
          <h2>Drop it in.<br />Take it further.</h2>
        </div>
        <div className="noir-stack">
          <div className="stack-card card-source"><FilePdf aria-hidden="true" /><span>source</span><b>research.pdf</b></div>
          <div className="stack-card card-process"><Lightning aria-hidden="true" /><span>process</span><b>layout + OCR</b></div>
          <div className="stack-card card-result"><FileText aria-hidden="true" /><span>artifact</span><b>research.md</b></div>
        </div>
      </section>
      <section className="noir-manifesto">
        <p><span>Shift is local-first.</span> Your source stays yours. The route stays visible. The output stays useful.</p>
        <CopyCommand />
      </section>
    </main>
  );
}

const studies = {
  terminal: TerminalDirection,
  workbench: WorkbenchDirection,
  signal: SignalDirection,
  receipt: ReceiptDirection,
  blueprint: BlueprintDirection,
  noir: NoirDirection,
};

export function App() {
  const [selected, setSelected] = useState(() => window.location.hash.slice(1) || "terminal");
  const Study = studies[selected] || TerminalDirection;

  useEffect(() => {
    window.history.replaceState(null, "", `#${selected}`);
    window.scrollTo({ top: 0, behavior: "instant" });
  }, [selected]);

  return (
    <div className={`app-shell direction-${selected}`}>
      <DirectionSwitcher selected={selected} onSelect={setSelected} />
      <div className="study-viewport" key={selected}>
        <Study />
      </div>
    </div>
  );
}
