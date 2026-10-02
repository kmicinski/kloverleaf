// LaTeX for Monaco: a Monarch tokenizer (commands, math, comments,
// environments), bracket behaviour, and completions for common commands,
// environments, and the project's own \label{}s and BibTeX keys.

const COMMANDS = [
  "section", "subsection", "subsubsection", "paragraph", "chapter", "part", "caption", "label", "ref", "eqref", "cite", "citep", "citet", "autoref", "cref",
  "textbf", "textit", "emph", "texttt", "textsc", "underline", "footnote", "url", "href", "includegraphics", "input", "include", "usepackage", "documentclass",
  "newcommand", "renewcommand", "newenvironment", "newtheorem", "frac", "sqrt", "sum", "prod", "int", "lim", "infty", "alpha", "beta", "gamma", "delta", "epsilon",
  "lambda", "mu", "sigma", "theta", "omega", "pi", "mathbb", "mathcal", "mathrm", "mathbf", "left", "right", "cdot", "cdots", "ldots", "vdots", "ddots", "times",
  "leq", "geq", "neq", "approx", "equiv", "subseteq", "subset", "in", "notin", "cup", "cap", "forall", "exists", "rightarrow", "leftarrow", "Rightarrow", "Leftrightarrow",
  "mapsto", "to", "partial", "nabla", "hat", "bar", "vec", "tilde", "dot", "overline", "underbrace", "text", "quad", "qquad", "hspace", "vspace", "noindent", "centering",
  "item", "maketitle", "tableofcontents", "bibliography", "bibliographystyle", "printbibliography", "begin", "end", "hline", "toprule", "midrule", "bottomrule",
  "multicolumn", "multirow", "textwidth", "linewidth", "columnwidth", "newpage", "clearpage", "pagebreak", "linebreak", "today", "LaTeX", "TeX", "ldots", "dots",
];

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
    triggerCharacters: ["\\", "{"],
    provideCompletionItems(model, position) {
      const line = model.getLineContent(position.lineNumber).slice(0, position.column - 1);
      const argMatch = /\\(ref|eqref|cref|autoref|cite|citep|citet|label|input|include|includegraphics)\{([^}]*)$/.exec(line);
      if (argMatch) {
        const [, cmd, typed] = argMatch;
        const start = position.column - typed.length;
        const range = new monaco.Range(position.lineNumber, start, position.lineNumber, position.column);
        const items = (cmd.startsWith("cite") ? bibKeys() : cmd.startsWith("ref") || cmd === "eqref" || cmd === "cref" || cmd === "autoref" ? labels() : []).map((k) => ({
          label: k, kind: monaco.languages.CompletionItemKind.Reference, insertText: k, range,
        }));
        return { suggestions: items };
      }
      const cmdMatch = /\\([A-Za-z@]*)$/.exec(line);
      if (!cmdMatch) return { suggestions: [] };
      const start = position.column - cmdMatch[1].length;
      const range = new monaco.Range(position.lineNumber, start, position.lineNumber, position.column);
      const suggestions: monaco.languages.CompletionItem[] = COMMANDS.map((c) => ({
        label: `\\${c}`, kind: monaco.languages.CompletionItemKind.Function, insertText: c, range, sortText: `1${c}`,
      }));
      for (const env of ENVIRONMENTS) {
        suggestions.push({
          label: `\\begin{${env}}`, kind: monaco.languages.CompletionItemKind.Snippet, sortText: `2${env}`, range,
          insertText: `begin{${env}}\n\t$0\n\\end{${env}}`,
          insertTextRules: monaco.languages.CompletionItemInsertTextRule.InsertAsSnippet,
        });
      }
      return { suggestions };
    },
  });
}

/** Every \label{…} across the open models. */
function labels(): string[] {
  const out = new Set<string>();
  for (const m of monaco.editor.getModels()) {
    for (const match of m.getValue().matchAll(/\\label\{([^}]+)\}/g)) out.add(match[1]);
  }
  return [...out].sort();
}

/** Every BibTeX key across the open .bib models. */
function bibKeys(): string[] {
  const out = new Set<string>();
  for (const m of monaco.editor.getModels()) {
    if (!m.uri.path.endsWith(".bib")) continue;
    for (const match of m.getValue().matchAll(/@\w+\s*\{\s*([^,\s]+)\s*,/g)) out.add(match[1]);
  }
  return [...out].sort();
}
