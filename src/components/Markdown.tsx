import { memo, useEffect, useId, useState } from "react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import remarkMath from "remark-math";
import rehypeKatex from "rehype-katex";
import "katex/dist/katex.min.css";
import { normalizeMath } from "../lib/math";
import { isExternalUrl, openExternal } from "../lib/external";
import { SessionImage } from "./AgentImage";
import { CheckIcon, CopyIcon } from "./icons";
import { FileLink, InLinkContext } from "./FileLink";

/** A link target as a path: `file://` URLs decoded, anything else as written. */
function fileHref(href: string): string {
  if (!/^file:\/\//i.test(href)) return href;
  try {
    return decodeURIComponent(href.replace(/^file:\/\/(localhost)?/i, ""));
  } catch {
    return href;
  }
}

const cache = new Map<string, string>();

const highlightKey = (code: string, lang: string) => `${lang}\u0000${code}`;

async function highlight(code: string, lang: string): Promise<string> {
  const key = highlightKey(code, lang);
  const hit = cache.get(key);
  if (hit) return hit;
  const { codeToHtml } = await import("shiki");
  const opts = { themes: { light: "github-light", dark: "github-dark" }, defaultColor: false as const };
  let html: string;
  try {
    html = await codeToHtml(code, { ...opts, lang: lang || "text" });
  } catch {
    html = await codeToHtml(code, { ...opts, lang: "text" });
  }
  if (cache.size > 500) cache.clear();
  cache.set(key, html);
  return html;
}

function isDark(): boolean {
  const t = document.documentElement.dataset.theme;
  return t ? t === "dark" : matchMedia("(prefers-color-scheme: dark)").matches;
}

let mermaidTheme: string | null = null;

/** Rendered diagrams by theme and source, so a row mounting again shows its diagram at once. */
const diagrams = new Map<string, string>();
const diagramKey = (code: string) => `${isDark() ? "dark" : "default"}\u0000${code}`;

async function renderMermaid(id: string, code: string): Promise<string | null> {
  const hit = diagrams.get(diagramKey(code));
  if (hit) return hit;
  const { default: mermaid } = await import("mermaid");
  const theme = isDark() ? "dark" : "default";
  if (mermaidTheme !== theme) {
    mermaid.initialize({ startOnLoad: false, securityLevel: "strict", theme });
    mermaidTheme = theme;
  }
  // Partial diagrams (mid-stream) fail to parse; keep showing the source until they're whole.
  if (!(await mermaid.parse(code, { suppressErrors: true }))) return null;
  try {
    const { svg } = await mermaid.render(id, code);
    if (diagrams.size > 100) diagrams.clear();
    diagrams.set(diagramKey(code), svg);
    return svg;
  } catch {
    document.getElementById(`d${id}`)?.remove();
    return null;
  }
}

function MermaidBlock({ code }: { code: string }) {
  const id = `mmd${useId().replace(/[^a-zA-Z0-9]/g, "")}`;
  // A diagram drawn before is shown from the first render: the row is measured at its real height.
  const [svg, setSvg] = useState<string | null>(() => diagrams.get(diagramKey(code)) ?? null);
  const [showCode, setShowCode] = useState(false);
  useEffect(() => {
    let live = true;
    const cached = diagrams.get(diagramKey(code));
    if (cached) {
      setSvg(cached);
      return;
    }
    const t = setTimeout(() => {
      renderMermaid(id, code).then((s) => live && setSvg(s));
    }, 300);
    return () => {
      live = false;
      clearTimeout(t);
    };
  }, [id, code]);
  if (!svg || showCode) {
    return <CodeBlock code={code} lang="mermaid" onToggleDiagram={svg ? () => setShowCode(false) : undefined} />;
  }
  return (
    <div className="code-block my-3 overflow-hidden rounded-xl border border-border bg-panel">
      <div className="flex h-9 items-center justify-between border-b border-line pl-3.5 pr-2 font-mono text-[12px] text-faint">
        <span>mermaid</span>
        <button className="rounded-md px-2 py-1 hover:bg-panel-2 hover:text-fg" onClick={() => setShowCode(true)}>
          Code
        </button>
      </div>
      <div className="flex justify-center overflow-x-auto bg-bg p-3" dangerouslySetInnerHTML={{ __html: svg }} />
    </div>
  );
}

export function CodeBlock({
  code,
  lang,
  onToggleDiagram,
}: {
  code: string;
  lang: string;
  onToggleDiagram?: () => void;
}) {
  // Code highlighted before is shown from the first render: a row that mounts again is measured at
  // its real height, not the plain one, so the transcript doesn't shift when the highlight lands.
  const [html, setHtml] = useState<string | null>(() => cache.get(highlightKey(code, lang)) ?? null);
  const [copied, setCopied] = useState(false);
  useEffect(() => {
    let live = true;
    const cached = cache.get(highlightKey(code, lang));
    if (cached) {
      setHtml(cached);
      return;
    }
    const t = setTimeout(() => {
      highlight(code, lang).then((h) => live && setHtml(h));
    }, 60);
    return () => {
      live = false;
      clearTimeout(t);
    };
  }, [code, lang]);
  return (
    <div className="code-block my-3 overflow-hidden rounded-xl border border-border bg-panel">
      <div className="flex h-9 items-center justify-between border-b border-line pl-3.5 pr-1.5 font-mono text-[12px] text-faint">
        <span>{lang || "text"}</span>
        <span className="flex-1" />
        {onToggleDiagram && (
          <button className="rounded-md px-2 py-1 hover:bg-panel-2 hover:text-fg" onClick={onToggleDiagram}>
            Diagram
          </button>
        )}
        <button
          className={`flex items-center gap-1.5 rounded-md px-2 py-1 hover:bg-panel-2 hover:text-fg ${copied ? "text-ok" : ""}`}
          onClick={() => {
            navigator.clipboard.writeText(code);
            setCopied(true);
            setTimeout(() => setCopied(false), 1200);
          }}
        >
          {copied ? <CheckIcon size={13} /> : <CopyIcon size={13} />}
          {copied ? "Copied" : "Copy"}
        </button>
      </div>
      {html ? (
        <div dangerouslySetInnerHTML={{ __html: html }} />
      ) : (
        <pre>
          <code>{code}</code>
        </pre>
      )}
    </div>
  );
}

/** A path on the session's computer, rather than a picture the page can load itself. */
function isLocalImageSrc(src: string | undefined): src is string {
  if (!src) return false;
  const s = src.trim();
  return s.length > 0 && !/^(https?:|data:|blob:|\/\/)/i.test(s);
}

function MarkdownImpl({ text, sessionId, keepBreaks }: { text: string; sessionId?: string; keepBreaks?: boolean }) {
  return (
    <div className={keepBreaks ? "prose-chat prose-chat-breaks" : "prose-chat"}>
      <ReactMarkdown
        remarkPlugins={[remarkGfm, [remarkMath, { singleDollarTextMath: false }]]}
        rehypePlugins={[[rehypeKatex, { throwOnError: false, strict: false }]]}
        components={{
          img: ({ src, alt }) => {
            const source = typeof src === "string" ? src : undefined;
            const name = typeof alt === "string" ? alt : undefined;
            if (sessionId && isLocalImageSrc(source)) return <SessionImage sessionId={sessionId} src={source} alt={name} />;
            return <img src={source} alt={name ?? ""} className="my-3 max-h-[28rem] max-w-full rounded-lg border border-line object-contain" />;
          },
          pre: ({ children }) => <>{children}</>,
          code: ({ className, children }) => {
            const m = /language-([\w+-]+)/.exec(className ?? "");
            const s = String(children ?? "");
            if (!m && !s.includes("\n"))
              return (
                <FileLink text={s}>
                  <code>{children}</code>
                </FileLink>
              );
            const code = s.replace(/\n$/, "");
            if (m?.[1] === "mermaid") return <MermaidBlock code={code} />;
            return <CodeBlock code={code} lang={m?.[1] ?? ""} />;
          },
          a: ({ href, children }) => {
            const inner = <InLinkContext.Provider value={true}>{children}</InLinkContext.Provider>;
            // A link to a file, e.g. `[spec](re/SPEC.md)`, opens it like a mentioned path does.
            if (href && !isExternalUrl(href)) return <FileLink text={fileHref(href)}>{inner}</FileLink>;
            return (
              <a
                href={href}
                title={href}
                rel="noreferrer"
                onClick={(e) => {
                  e.preventDefault();
                  if (isExternalUrl(href)) openExternal(href);
                }}
              >
                {inner}
              </a>
            );
          },
        }}
      >
        {normalizeMath(text)}
      </ReactMarkdown>
    </div>
  );
}

export const Markdown = memo(MarkdownImpl);
