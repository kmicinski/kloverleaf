// LaTeX for Monaco: a Monarch tokenizer (commands, math, comments,
// environments), bracket behaviour, and completions for common commands,
// environments, and the project's own \label{}s and BibTeX keys.

const ENVIRONMENTS = [
  "document", "itemize", "enumerate", "description", "figure", "table", "tabular", "equation", "equation*", "align", "align*", "gather", "cases", "matrix", "pmatrix",
  "bmatrix", "theorem", "lemma", "proof", "definition", "corollary", "proposition", "example", "remark", "abstract", "center", "verbatim", "lstlisting", "minted",
  "tikzpicture", "frame", "quote", "minipage", "subfigure", "algorithm", "algorithmic", "thebibliography",
];

export function registerLatex(): void {
  monaco.languages.register({ id: "latex", extensions: [".tex", ".sty", ".cls", ".bib"] });
  monaco.languages.setLanguageConfiguration("latex", {
    comments: { lineComment: "%" },
    brackets: [["{", "}"], ["[", "]"], ["(", ")"]],
    autoClosingPairs: [
      { open: "{", close: "}" }, { open: "[", close: "]" }, { open: "(", close: ")" },
      { open: "$", close: "$", notIn: ["comment"] }, { open: "`", close: "'" },
    ],
    surroundingPairs: [{ open: "{", close: "}" }, { open: "[", close: "]" }, { open: "(", close: ")" }, { open: "$", close: "$" }],
    wordPattern: /\\?[A-Za-z@]+\*?|[^\s{}\[\]()$\\%]+/g,
    folding: { markers: { start: /\\begin\{/, end: /\\end\{/ } },
  });
  monaco.languages.setMonarchTokensProvider("latex", {
    tokenizer: {
      root: [
        [/%.*$/, "comment"],
        [/(\\begin|\\end)(\{)([^}]*)(\})/, ["keyword", "delimiter", "type", "delimiter"]],
        [/\\(section|subsection|subsubsection|chapter|part|paragraph)\*?/, "markup.heading"],
        [/\\(documentclass|usepackage|input|include|bibliography)\b/, "keyword.control"],
        [/\\\[/, { token: "string", next: "@displaymath" }],
        [/\$\$/, { token: "string", next: "@displaymath2" }],
        [/\\\(/, { token: "string", next: "@inlinemath" }],
        [/\$/, { token: "string", next: "@inlinedollar" }],
        [/\\[A-Za-z@]+\*?/, "keyword"],
        [/\\./, "keyword"],
        [/[{}]/, "delimiter.curly"],
        [/[\[\]]/, "delimiter.square"],
        [/\d+(\.\d+)?(pt|cm|mm|em|ex|in|bp)?/, "number"],
        [/[&~^_]/, "operator"],
      ],
      displaymath: [[/\\\]/, { token: "string", next: "@pop" }], { include: "@math" }],
      displaymath2: [[/\$\$/, { token: "string", next: "@pop" }], { include: "@math" }],
      inlinemath: [[/\\\)/, { token: "string", next: "@pop" }], { include: "@math" }],
      inlinedollar: [[/\$/, { token: "string", next: "@pop" }], { include: "@math" }],
      math: [
        [/%.*$/, "comment"],
        [/\\[A-Za-z@]+/, "variable"],
        [/\\./, "variable"],
        [/[{}\[\]()]/, "delimiter"],
        [/[^\\$%{}\[\]()]+/, "string"],
      ],
    },
  });
  monaco.languages.registerCompletionItemProvider("latex", {
    triggerCharacters: ["\\", "{", ",", "["],
    provideCompletionItems(model, position) {
      const line = model.getLineContent(position.lineNumber).slice(0, position.column - 1);
      const rest = model.getLineContent(position.lineNumber).slice(position.column - 1);
      const K = monaco.languages.CompletionItemKind;
      const Snip = monaco.languages.CompletionItemInsertTextRule.InsertAsSnippet;
      // Inside the argument of a reference-like command: \ref{…}, \cite{a,b…}, \begin{…}, \includegraphics{…}
      const arg = /\\(ref|eqref|cref|Cref|autoref|pageref|vref|cite|citep|citet|citeauthor|citeyear|autocite|textcite|parencite|label|input|include|includegraphics|usepackage|begin|end|bibliography|documentclass)(\[[^\]]*\])?\{([^}]*)$/.exec(line);
      if (arg) {
        const [, cmd, , inside] = arg;
        const typed = inside.split(",").pop()!.trim();
        const start = position.column - typed.length;
        const range = new monaco.Range(position.lineNumber, start, position.lineNumber, position.column);
        const closes = rest.startsWith("}") ? 0 : 1; // auto-closed brace already there?
        const brace = closes ? "}" : "";
        let items: monaco.languages.CompletionItem[] = [];
        if (/^cite|^autocite|^textcite|^parencite/.test(cmd)) {
          items = bibEntries().map((e) => ({ label: e.key, kind: K.Reference, insertText: e.key, range, detail: e.detail, documentation: e.doc, sortText: `0${e.key}`, filterText: `${e.key} ${e.detail}` }));
        } else if (/ref$|^label$/.test(cmd) || cmd === "pageref" || cmd === "vref") {
          items = labelEntries().map((l) => ({ label: l.name, kind: K.Reference, insertText: l.name + brace, range, detail: l.detail, sortText: `0${l.name}` }));
        } else if (cmd === "begin") {
          // The env name plus its \end — unless the \end is already below.
          const after = model.getValue().slice(model.getOffsetAt(position));
          items = environments().map((env) => {
            const hasEnd = new RegExp(`\\\\end\\{${env.replace("*", "\\*")}\\}`).test(after.slice(0, 2000));
            const body = ENV_BODY[env] ?? "\t$0";
            return { label: env, kind: K.Module, range, sortText: `0${env}`,
              insertText: hasEnd ? env + brace : `${env}${brace}\n${body}\n\\end{${env}}`, insertTextRules: Snip };
          });
        } else if (cmd === "end") {
          const before = model.getValue().slice(0, model.getOffsetAt(position));
          const open = openEnvironments(before);
          items = [...open.map((env, i) => ({ label: env, kind: K.Module, insertText: env + brace, range, sortText: `0${String(i).padStart(2, "0")}`, detail: "open environment" })),
            ...environments().filter((e) => !open.includes(e)).map((env) => ({ label: env, kind: K.Module, insertText: env + brace, range, sortText: `1${env}` }))];
        } else if (cmd === "includegraphics" || cmd === "input" || cmd === "include" || cmd === "bibliography") {
          const want = cmd === "includegraphics" ? /\.(pdf|png|jpe?g|eps|svg)$/i : cmd === "bibliography" ? /\.bib$/i : /\.tex$/i;
          items = projectFiles.filter((f) => want.test(f)).map((f) => {
            const name = cmd === "includegraphics" ? f : f.replace(/\.(tex|bib)$/i, "");
            return { label: f, kind: K.File, insertText: name + brace, range, sortText: `0${f}` };
          });
        } else if (cmd === "usepackage") {
          items = PACKAGES.map((p) => ({ label: p, kind: K.Module, insertText: p + brace, range, sortText: `0${p}` }));
        } else if (cmd === "documentclass") {
          items = ["article", "report", "book", "letter", "beamer", "acmart", "IEEEtran", "llncs", "amsart", "memoir", "scrartcl", "standalone"].map((c) => ({ label: c, kind: K.Module, insertText: c + brace, range }));
        }
        return { suggestions: items };
      }
      // A command being typed: \sec… → \section{…}
      const cmdMatch = /\\([A-Za-z@]*)$/.exec(line);
      if (!cmdMatch) return { suggestions: [] };
      const start = position.column - cmdMatch[1].length;
      const range = new monaco.Range(position.lineNumber, start, position.lineNumber, position.column);
      const seen = usedCommands();
      const suggestions: monaco.languages.CompletionItem[] = [];
      const added = new Set<string>();
      for (const [name, snippet, detail] of commandSnippets()) {
        added.add(name);
        const used = seen.get(name) ?? 0;
        suggestions.push({ label: `\\${name}`, kind: snippet.includes("$") ? K.Snippet : K.Function, insertText: snippet, insertTextRules: Snip, range,
          detail, sortText: `${used ? "0" : "1"}${(999 - Math.min(used, 999)).toString().padStart(3, "0")}${name}` });
      }
      // Commands this document uses or defines that the list does not know.
      for (const [name, n] of seen) {
        if (added.has(name)) continue;
        suggestions.push({ label: `\\${name}`, kind: K.Function, insertText: name, range, detail: `used ${n}× here`, sortText: `0${(999 - Math.min(n, 999)).toString().padStart(3, "0")}${name}` });
      }
      for (const env of environments()) {
        suggestions.push({ label: `\\begin{${env}}`, kind: K.Snippet, sortText: `2${env}`, range, detail: "environment",
          insertText: `begin{${env}}\n${ENV_BODY[env] ?? "\t$0"}\n\\end{${env}}`, insertTextRules: Snip });
      }
      return { suggestions };
    },
  });
}

// ---- hovers and go-to-definition ------------------------------------------------
//
// Hover a \ref to see the caption it points at, a \cite to see the entry, an
// \includegraphics to see the picture; F12 / ⌘-click jumps to the \label, the
// BibTeX entry, or the file. Monaco finds the other models by URI; the editor
// registers an opener (registerEditorOpener) that shows them.

/** The command and argument under a position: `\ref{fig:a}` → {cmd, arg, argRange}. */
function argumentAt(model: monaco.editor.ITextModel, position: monaco.Position): { cmd: string; arg: string; range: monaco.Range } | null {
  const line = model.getLineContent(position.lineNumber);
  const re = /\\([A-Za-z]+)\*?(?:\[[^\]]*\])?\{([^}]*)\}/g;
  for (let x = re.exec(line); x; x = re.exec(line)) {
    const start = x.index + x[0].indexOf("{") + 2, end = start + x[2].length;
    if (position.column >= start && position.column <= end) {
      // Inside a comma list (\cite{a,b}) pick the item under the cursor.
      let a = start, b = end;
      const items = x[2].split(",");
      let at = start;
      for (const it of items) {
        if (position.column >= at && position.column <= at + it.length) { a = at; b = at + it.length; break; }
        at += it.length + 1;
      }
      return { cmd: x[1], arg: line.slice(a - 1, b - 1).trim(), range: new monaco.Range(position.lineNumber, a, position.lineNumber, b) };
    }
  }
  return null;
}

const REFS = /^(ref|eqref|cref|Cref|autoref|pageref|vref|nameref)$/;
const CITES = /^(cite|citep|citet|citeauthor|citeyear|autocite|textcite|parencite|footcite|nocite)$/;
const FILES = /^(includegraphics|input|include|bibliography|addbibresource)$/;

function fileOf(model: monaco.editor.ITextModel): string { return model.uri.path.replace(/^\//, ""); }
function modelFor(file: string): monaco.editor.ITextModel | undefined { return monaco.editor.getModels().find((m) => fileOf(m) === file); }

/** Where `\label{name}` is: model, line, column. */
function labelLocation(name: string): { model: monaco.editor.ITextModel; line: number; column: number } | null {
  const needle = `\\label{${name}}`;
  for (const m of monaco.editor.getModels()) {
    const text = m.getValue();
    const i = text.indexOf(needle);
    if (i >= 0) { const p = m.getPositionAt(i); return { model: m, line: p.lineNumber, column: p.column }; }
  }
  return null;
}

/** Where a BibTeX entry is, and its text. */
function bibLocation(key: string): { model: monaco.editor.ITextModel; line: number; column: number; entry: string } | null {
  for (const m of monaco.editor.getModels()) {
    if (!m.uri.path.endsWith(".bib")) continue;
    const text = m.getValue();
    const re = new RegExp(`@\\w+\\s*\\{\\s*${key.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}\\s*,`);
    const x = re.exec(text);
    if (!x) continue;
    // The entry runs to the matching closing brace.
    let depth = 0, j = x.index;
    for (; j < text.length; j++) { if (text[j] === "{") depth++; else if (text[j] === "}") { depth--; if (depth === 0) { j++; break; } } }
    const p = m.getPositionAt(x.index);
    return { model: m, line: p.lineNumber, column: p.column, entry: text.slice(x.index, Math.min(j, x.index + 1500)) };
  }
  return null;
}

/** The project files this module may link to, with their text-ness, from the editor. */
function resolveFile(arg: string, cmd: string): string | null {
  const candidates = cmd === "includegraphics" ? [arg, `${arg}.pdf`, `${arg}.png`, `${arg}.jpg`, `${arg}.jpeg`, `${arg}.eps`] : cmd === "bibliography" || cmd === "addbibresource" ? [arg, `${arg}.bib`] : [arg, `${arg}.tex`];
  return candidates.find((c) => projectFiles.includes(c)) ?? null;
}

export function registerLatexNavigation(project: string): void {
  monaco.languages.registerHoverProvider("latex", {
    provideHover(model, position) {
      const hit = argumentAt(model, position);
      if (!hit) return null;
      const md = (v: string) => ({ contents: [{ value: v, isTrusted: true, supportHtml: true }], range: hit.range });
      if (REFS.test(hit.cmd)) {
        const loc = labelLocation(hit.arg);
        if (!loc) return md(`**${hit.arg}** — no \\label with this name _(⌘-click a label name elsewhere to define it)_`);
        const text = loc.model.getValue();
        const at = loc.model.getOffsetAt({ lineNumber: loc.line, column: loc.column });
        const before = text.slice(Math.max(0, at - 300), at);
        const cap = /\\caption\{([^}]{0,200})/.exec(before)?.[1] ?? /\\(?:sub)*section\*?\{([^}]{0,120})/.exec(before)?.[1];
        const what = cap ? cap.replace(/\\[A-Za-z]+\{?/g, "").replace(/[{}]/g, "") : loc.model.getLineContent(loc.line).trim();
        return md(`**${hit.arg}** · ${fileOf(loc.model)}:${loc.line}\n\n${what}\n\n_F12 or ⌘-click to go there_`);
      }
      if (CITES.test(hit.cmd)) {
        const loc = bibLocation(hit.arg);
        if (!loc) return md(`**${hit.arg}** — no BibTeX entry with this key in the open .bib files`);
        return md(`**${hit.arg}** · ${fileOf(loc.model)}:${loc.line}\n\n\`\`\`bibtex\n${loc.entry}\n\`\`\``);
      }
      if (FILES.test(hit.cmd)) {
        const file = resolveFile(hit.arg, hit.cmd);
        if (!file) return md(`**${hit.arg}** — not in the project`);
        if (/\.(png|jpe?g|gif|svg|webp)$/i.test(file)) {
          return md(`**${file}**\n\n<img src="/api/projects/${project}/files/${file}" style="max-width:360px;max-height:280px" />`);
        }
        return md(`**${file}**${/\.pdf$/i.test(file) ? " · PDF figure" : ""}\n\n_F12 or ⌘-click to open_`);
      }
      return null;
    },
  });
  monaco.languages.registerDefinitionProvider("latex", {
    provideDefinition(model, position) {
      const hit = argumentAt(model, position);
      if (!hit) return null;
      if (REFS.test(hit.cmd)) {
        const loc = labelLocation(hit.arg);
        return loc ? { uri: loc.model.uri, range: new monaco.Range(loc.line, loc.column, loc.line, loc.column + hit.arg.length + 8) } : null;
      }
      if (CITES.test(hit.cmd)) {
        const loc = bibLocation(hit.arg);
        return loc ? { uri: loc.model.uri, range: new monaco.Range(loc.line, loc.column, loc.line, loc.column + hit.arg.length + 2) } : null;
      }
      if (FILES.test(hit.cmd)) {
        const file = resolveFile(hit.arg, hit.cmd);
        const m = file && modelFor(file);
        if (m) return { uri: m.uri, range: new monaco.Range(1, 1, 1, 1) };
        if (file) return { uri: monaco.Uri.parse(`kloverleaf:/${file}`), range: new monaco.Range(1, 1, 1, 1) };
      }
      return null;
    },
  });
}

/** Label names and BibTeX keys, for quick fixes ("did you mean …?"). */
export function knownLabels(): string[] { return labelEntries().map((l) => l.name); }
export function knownBibKeys(): string[] { return bibEntries().map((e) => e.key); }

/** Edit distance, for "did you mean". */
export function levenshtein(a: string, b: string): number {
  const dp = Array.from({ length: a.length + 1 }, (_, i) => [i, ...new Array<number>(b.length).fill(0)]);
  for (let j = 1; j <= b.length; j++) dp[0][j] = j;
  for (let i = 1; i <= a.length; i++) for (let j = 1; j <= b.length; j++) dp[i][j] = Math.min(dp[i - 1][j] + 1, dp[i][j - 1] + 1, dp[i - 1][j - 1] + (a[i - 1] === b[j - 1] ? 0 : 1));
  return dp[a.length][b.length];
}

// ---- what the completions know ------------------------------------------------

let projectFiles: string[] = [];
/** The editor tells us the project's files (for \includegraphics, \input, \bibliography). */
export function setProjectFiles(files: string[]): void { projectFiles = files; }

const PACKAGES = ["amsmath", "amssymb", "amsthm", "graphicx", "hyperref", "cleveref", "booktabs", "geometry", "xcolor", "tikz", "pgfplots", "subcaption", "caption", "enumitem", "natbib", "biblatex", "listings", "minted", "algorithm", "algorithmic", "algorithm2e", "url", "microtype", "fontenc", "inputenc", "babel", "lmodern", "times", "multirow", "multicol", "wrapfig", "float", "siunitx", "mathtools", "bm", "todonotes", "xspace", "array", "tabularx", "longtable", "fancyhdr", "titlesec", "setspace", "csquotes", "fontspec", "unicode-math", "tcolorbox", "framed", "mdframed", "comment", "lipsum", "balance", "flushend"];

/** Bodies for environment snippets (tab-indented; the cursor lands at $0). */
const ENV_BODY: Record<string, string> = {
  itemize: "\t\\item $0", enumerate: "\t\\item $0", description: "\t\\item[$1] $0",
  figure: "\t\\centering\n\t\\includegraphics[width=${1:0.8}\\textwidth]{$2}\n\t\\caption{$3}\n\t\\label{fig:$4}$0",
  table: "\t\\centering\n\t\\begin{tabular}{${1:lcc}}\n\t\t\\toprule\n\t\t$0 \\\\\n\t\t\\bottomrule\n\t\\end{tabular}\n\t\\caption{$2}\n\t\\label{tab:$3}",
  tabular: "\t$0 \\\\",
  equation: "\t$0", "equation*": "\t$0", align: "\t$0", "align*": "\t$0", gather: "\t$0",
  cases: "\t$1 & $2 \\\\\n\t$0", matrix: "\t$0", pmatrix: "\t$0", bmatrix: "\t$0",
  wrapfigure: "\t\\centering\n\t\\includegraphics[width=${1:0.45}\\textwidth]{$2}\n\t\\caption{$3}\n\t\\label{fig:$4}$0",
  minipage: "\t$0", frame: "\t\\frametitle{$1}\n\t$0", abstract: "\t$0", proof: "\t$0", theorem: "\t$0", lemma: "\t$0",
  algorithmic: "\t\\STATE $0", lstlisting: "$0", verbatim: "$0", tikzpicture: "\t$0",
};

/** Commands with argument snippets; plain names stand alone. */
function commandSnippets(): [string, string, string][] {
  const out: [string, string, string][] = [];
  const snip = (name: string, body: string, detail: string) => out.push([name, body, detail]);
  for (const s of ["section", "subsection", "subsubsection", "paragraph", "chapter", "part"]) snip(s, `${s}{$1}$0`, "heading");
  for (const s of ["textbf", "textit", "emph", "texttt", "textsc", "underline", "text", "mathrm", "mathbf", "mathcal", "mathbb", "footnote", "caption", "url", "hat", "bar", "vec", "tilde", "overline", "sqrt", "mbox", "textsf", "textcolor", "title", "author"]) snip(s, `${s}{$1}$0`, "");
  snip("label", "label{$1}$0", "label"); snip("ref", "ref{$1}$0", "reference"); snip("eqref", "eqref{$1}$0", "reference"); snip("cref", "cref{$1}$0", "cleveref"); snip("Cref", "Cref{$1}$0", "cleveref"); snip("autoref", "autoref{$1}$0", "hyperref");
  snip("cite", "cite{$1}$0", "citation"); snip("citep", "citep{$1}$0", "natbib"); snip("citet", "citet{$1}$0", "natbib"); snip("autocite", "autocite{$1}$0", "biblatex"); snip("textcite", "textcite{$1}$0", "biblatex");
  snip("href", "href{$1}{$2}$0", "link"); snip("frac", "frac{$1}{$2}$0", "math"); snip("dfrac", "dfrac{$1}{$2}$0", "math"); snip("binom", "binom{$1}{$2}$0", "math");
  snip("includegraphics", "includegraphics[width=${1:0.8}\\textwidth]{$2}$0", "figure"); snip("input", "input{$1}$0", ""); snip("include", "include{$1}$0", "");
  snip("usepackage", "usepackage{$1}$0", "preamble"); snip("documentclass", "documentclass[${1:11pt}]{${2:article}}$0", "preamble");
  snip("newcommand", "newcommand{\\\\$1}{$2}$0", "macro"); snip("renewcommand", "renewcommand{\\\\$1}{$2}$0", "macro"); snip("newtheorem", "newtheorem{$1}{$2}$0", "");
  snip("begin", "begin{$1}\n\t$0\n\\end{$1}", "environment"); snip("end", "end{$1}$0", "");
  snip("item", "item $0", "list"); snip("hspace", "hspace{$1}$0", ""); snip("vspace", "vspace{$1}$0", ""); snip("multicolumn", "multicolumn{$1}{$2}{$3}$0", "table"); snip("multirow", "multirow{$1}{*}{$2}$0", "table");
  snip("left", "left$1 $0 \\\\right$1", "math"); snip("sum", "sum_{$1}^{$2} $0", "math"); snip("prod", "prod_{$1}^{$2} $0", "math"); snip("int", "int_{$1}^{$2} $0", "math"); snip("lim", "lim_{$1} $0", "math");
  snip("underbrace", "underbrace{$1}_{$2}$0", "math"); snip("overbrace", "overbrace{$1}^{$2}$0", "math"); snip("bibliography", "bibliography{$1}$0", ""); snip("bibliographystyle", "bibliographystyle{${1:plain}}$0", "");
  snip("todo", "todo{$1}$0", "todonotes"); snip("SI", "SI{$1}{$2}$0", "siunitx"); snip("num", "num{$1}$0", "siunitx");
  const plain = ["maketitle", "tableofcontents", "printbibliography", "hline", "toprule", "midrule", "bottomrule", "centering", "noindent", "newpage", "clearpage", "pagebreak", "linebreak", "today", "LaTeX", "TeX", "ldots", "cdots", "vdots", "ddots", "dots", "quad", "qquad", "textwidth", "linewidth", "columnwidth", "infty", "alpha", "beta", "gamma", "delta", "epsilon", "varepsilon", "zeta", "eta", "theta", "iota", "kappa", "lambda", "mu", "nu", "xi", "pi", "rho", "sigma", "tau", "upsilon", "phi", "varphi", "chi", "psi", "omega", "Gamma", "Delta", "Theta", "Lambda", "Xi", "Pi", "Sigma", "Phi", "Psi", "Omega", "cdot", "times", "leq", "geq", "neq", "approx", "equiv", "subseteq", "subset", "supseteq", "in", "notin", "cup", "cap", "forall", "exists", "rightarrow", "leftarrow", "Rightarrow", "Leftarrow", "Leftrightarrow", "leftrightarrow", "mapsto", "to", "partial", "nabla", "land", "lor", "lnot", "neg", "emptyset", "varnothing", "mid", "circ", "star", "ast", "pm", "mp", "oplus", "otimes", "setminus", "top", "bot", "vdash", "models", "langle", "rangle", "lfloor", "rfloor", "lceil", "rceil", "ell", "hbar", "displaystyle", "small", "footnotesize", "scriptsize", "tiny", "large", "Large", "LARGE", "huge", "Huge", "bfseries", "itshape", "ttfamily", "em", "par", "smallskip", "medskip", "bigskip", "hfill", "vfill", "hrule", "indent", "and", "thanks", "date", "newline", "relax", "protect", "makeatletter", "makeatother"];
  for (const s of plain) snip(s, s, "");
  return out;
}

/** Known environments plus any the document uses. */
function environments(): string[] {
  const out = new Set(ENVIRONMENTS);
  for (const m of monaco.editor.getModels()) for (const x of m.getValue().matchAll(/\\begin\{([A-Za-z*]+)\}/g)) out.add(x[1]);
  return [...out].sort();
}

/** Environments opened but not yet closed above the cursor, innermost first. */
function openEnvironments(before: string): string[] {
  const stack: string[] = [];
  for (const x of before.matchAll(/\\(begin|end)\{([A-Za-z*]+)\}/g)) {
    if (x[1] === "begin") stack.push(x[2]);
    else { const i = stack.lastIndexOf(x[2]); if (i >= 0) stack.splice(i, 1); }
  }
  return stack.reverse();
}

/** How often each command appears across the open models (sorting: what you use comes first). */
function usedCommands(): Map<string, number> {
  const out = new Map<string, number>();
  for (const m of monaco.editor.getModels()) {
    if (!/\.(tex|sty|cls)$/i.test(m.uri.path)) continue;
    for (const x of m.getValue().matchAll(/\\([A-Za-z@]{2,})/g)) out.set(x[1], (out.get(x[1]) ?? 0) + 1);
    for (const x of m.getValue().matchAll(/\\(?:re)?newcommand\{?\\([A-Za-z@]+)/g)) out.set(x[1], (out.get(x[1]) ?? 0) + 5);
  }
  for (const k of ["begin", "end"]) out.delete(k);
  return out;
}

/** Every \label{…} with the text around it, so the menu says what it points at. */
function labelEntries(): { name: string; detail: string }[] {
  const out = new Map<string, string>();
  for (const m of monaco.editor.getModels()) {
    const text = m.getValue();
    for (const x of text.matchAll(/\\label\{([^}]+)\}/g)) {
      const at = x.index ?? 0;
      const around = text.slice(Math.max(0, at - 160), at);
      const cap = /\\caption\{([^}]{0,80})/.exec(around)?.[1] ?? /\\(?:sub)*section\*?\{([^}]{0,80})/.exec(around)?.[1];
      const lineStart = text.lastIndexOf("\n", at) + 1;
      const detail = (cap ?? text.slice(lineStart, at).trim()).replace(/\\[A-Za-z]+\{?/g, "").replace(/[{}]/g, "").trim().slice(0, 60);
      out.set(x[1], detail);
    }
  }
  return [...out].map(([name, detail]) => ({ name, detail })).sort((a, b) => a.name.localeCompare(b.name));
}

/** BibTeX entries across the open .bib models, with author · year · title for the menu. */
function bibEntries(): { key: string; detail: string; doc: string }[] {
  const out: { key: string; detail: string; doc: string }[] = [];
  for (const m of monaco.editor.getModels()) {
    if (!m.uri.path.endsWith(".bib")) continue;
    const text = m.getValue();
    for (const x of text.matchAll(/@(\w+)\s*\{\s*([^,\s]+)\s*,/g)) {
      const at = x.index ?? 0;
      const body = text.slice(at, at + 2000);
      const field = (f: string) => new RegExp(`${f}\\s*=\\s*[{"]([^}"]*)`, "i").exec(body)?.[1]?.replace(/[{}]/g, "").trim();
      const author = field("author")?.split(/\s+and\s+/)[0]?.split(",")[0] ?? "";
      const year = field("year") ?? "";
      const title = field("title") ?? "";
      out.push({ key: x[2], detail: [author, year].filter(Boolean).join(" "), doc: title });
    }
  }
  return out.sort((a, b) => a.key.localeCompare(b.key));
}
