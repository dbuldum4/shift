import { useState } from "react";

const statement =
  "shift is a blazingly fast, native, opinionated, robust, and resilient file converter built for macOS";

const githubUrl = "https://github.com/dbuldum4/shift";
const releaseUrl = `${githubUrl}/releases`;

function ExternalLink({ href, children, className = "" }) {
  return (
    <a className={className} href={href} target="_blank" rel="noreferrer">
      {children} ↗
    </a>
  );
}

function CopyCommand() {
  const command = "cargo install shift-cli";
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

export function App() {
  return (
    <main id="top">
      <nav className="page-nav" aria-label="page navigation">
        <a className="wordmark" href="#top">shift</a>
        <ExternalLink href={githubUrl}>github</ExternalLink>
      </nav>

      <section className="hero" aria-labelledby="intro">
        <h1 id="intro">{statement}</h1>
        <div className="actions">
          <ExternalLink className="download-link" href={releaseUrl}>download shift</ExternalLink>
          <CopyCommand />
        </div>
      </section>

      <footer>
        <div className="routes" aria-label="conversion engines">
          <p><b>documents</b><span>docling · pandoc · markitdown</span></p>
          <p><b>web</b><span>defuddle</span></p>
          <p><b>media</b><span>ffmpeg</span></p>
        </div>
        <small>macOS 13+ · source preserved · native app + cli</small>
      </footer>
    </main>
  );
}
