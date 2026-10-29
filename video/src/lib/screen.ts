// A small terminal emulator for recorded output: printable text with
// autowrap, carriage return and newline, SGR styling, and erase-in-line.
// Enough for what sats prints; anything else is ignored, never guessed.

export type Style = {
  bold: boolean;
  dim: boolean;
  // A palette name ("green", "yellow", "red") or a CSS color.
  fg: string | null;
};

export type Cell = { ch: string; style: Style };

export type Span = { text: string; style: Style };

const PLAIN: Style = { bold: false, dim: false, fg: null };

const ANSI_COLORS: Record<number, string> = {
  30: "black",
  31: "red",
  32: "green",
  33: "yellow",
  34: "blue",
  35: "magenta",
  36: "cyan",
  37: "white",
};

const sameStyle = (a: Style, b: Style) =>
  a.bold === b.bold && a.dim === b.dim && a.fg === b.fg;

export class Screen {
  lines: Cell[][] = [[]];
  row = 0;
  col = 0;
  private style: Style = PLAIN;
  private pending = "";

  constructor(readonly width: number) {}

  write(data: string): this {
    const text = this.pending + data;
    this.pending = "";
    let i = 0;
    while (i < text.length) {
      const ch = text[i];
      if (ch === "\x1b") {
        const match = /^\x1b\[([0-9;?]*)([@-~])/.exec(text.slice(i));
        if (!match) {
          // An escape split across writes: finish it with the next write.
          this.pending = text.slice(i);
          return this;
        }
        this.csi(match[1], match[2]);
        i += match[0].length;
        continue;
      }
      if (ch === "\r") {
        this.col = 0;
      } else if (ch === "\n") {
        this.row += 1;
        this.line();
      } else if (ch === "\b") {
        this.col = Math.max(0, this.col - 1);
      } else if (ch >= " ") {
        this.put(ch);
      }
      i += 1;
    }
    return this;
  }

  /** Each line as runs of equally styled text. */
  spans(): Span[][] {
    return this.lines.map((cells) => {
      const spans: Span[] = [];
      for (const cell of cells) {
        const last = spans[spans.length - 1];
        if (last && sameStyle(last.style, cell.style)) {
          last.text += cell.ch;
        } else {
          spans.push({ text: cell.ch, style: cell.style });
        }
      }
      return spans;
    });
  }

  text(row: number): string {
    return (this.lines[row] ?? []).map((cell) => cell.ch).join("");
  }

  private line(): Cell[] {
    while (this.lines.length <= this.row) {
      this.lines.push([]);
    }
    return this.lines[this.row];
  }

  private put(ch: string) {
    if (this.col >= this.width) {
      this.row += 1;
      this.col = 0;
    }
    const line = this.line();
    while (line.length < this.col) {
      line.push({ ch: " ", style: PLAIN });
    }
    line[this.col] = { ch, style: this.style };
    this.col += 1;
  }

  private csi(params: string, final: string) {
    const args = params.split(";").map((p) => (p === "" ? 0 : Number(p)));
    if (final === "m") {
      this.sgr(args);
    } else if (final === "K") {
      const line = this.line();
      if (args[0] === 2) {
        line.length = 0;
      } else if (args[0] === 0) {
        line.length = Math.min(line.length, this.col);
      }
    } else if (final === "G") {
      this.col = Math.max(0, (args[0] || 1) - 1);
    } else if (final === "C") {
      this.col += args[0] || 1;
    } else if (final === "D") {
      this.col = Math.max(0, this.col - (args[0] || 1));
    }
  }

  private sgr(args: number[]) {
    let style = { ...this.style };
    for (let i = 0; i < args.length; i++) {
      const code = args[i];
      if (code === 0) {
        style = { ...PLAIN };
      } else if (code === 1) {
        style.bold = true;
      } else if (code === 2) {
        style.dim = true;
      } else if (code === 22) {
        style.bold = false;
        style.dim = false;
      } else if (code === 39) {
        style.fg = null;
      } else if (ANSI_COLORS[code]) {
        style.fg = ANSI_COLORS[code];
      } else if (code >= 90 && code <= 97) {
        style.fg = ANSI_COLORS[code - 60];
      } else if (code === 38 && args[i + 1] === 2) {
        style.fg = `rgb(${args[i + 2]}, ${args[i + 3]}, ${args[i + 4]})`;
        i += 4;
      }
    }
    this.style = style;
  }
}
