// Everyone gets a colour — the same one everywhere (header chips, cursors in
// the text, chat names, comment pins), the same on every page: a hash of the
// login picks one of the Solarized accents.

export const COLORS = ["#268bd2", "#d33682", "#859900", "#cb4b16", "#6c71c4", "#2aa198", "#b58900", "#dc322f"];

export function colorIndex(user: string): number {
  let h = 0x811c9dc5;
  for (const ch of user) { h ^= ch.codePointAt(0)!; h = Math.imul(h, 0x01000193); }
  return (h >>> 0) % COLORS.length;
}

export const colorOf = (user: string): string => COLORS[colorIndex(user)];

/** A CSS class name safe for any login. */
export const userClass = (user: string): string => `u-${[...user].map((c) => (/[a-z0-9]/i.test(c) ? c : `_${c.codePointAt(0)}`)).join("")}`;

const styled = new Set<string>();
/** Per-person rules (a caret in their colour with their name on it), added once. */
export function ensureUserStyle(user: string): string {
  const cls = userClass(user);
  if (styled.has(cls)) return cls;
  styled.add(cls);
  const c = colorOf(user);
  const name = user.replace(/["\\]/g, "");
  const style = document.createElement("style");
  style.textContent = `
.rc-caret.${cls} { border-left: 2px solid ${c}; margin-left: -1px; }
.rc-caret.${cls}::after { content: "${name}"; background: ${c}; }
.rc-line.${cls} { background: ${c}14; }
.chat-glyph.${cls} { background: ${c}; }`;
  document.head.appendChild(style);
  return cls;
}
