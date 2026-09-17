// Client-side counterpart to the arithmetic grammar in
// backend/src/expr.rs (`+ - * /`, unary +/-, parentheses, decimal
// literals). The two are not connected at runtime: no API route calls
// into expr.rs, so evalMath is what actually resolves a "10+5.5"-style
// amount field before it is sent to the backend as a plain number; the
// backend only ever receives the already-evaluated numeric result.
//
// The Android app serves this page under a Content Security Policy with no
// 'unsafe-eval', so this file must never construct or evaluate a string as
// JavaScript (no `Function`, no `eval`). It tokenizes and evaluates the
// grammar by hand instead.

// Grammar (standard precedence, left-associative binary operators),
// mirroring backend/src/expr.rs:
//
//   expr   := term  (('+' | '-') term)*
//   term   := unary (('*' | '/') unary)*
//   unary  := ('+' | '-') unary | atom
//   atom   := NUMBER | '(' expr ')'

type Token = { kind: "number"; value: number } | { kind: "+" | "-" | "*" | "/" | "(" | ")" };

const SINGLE_CHAR_TOKENS: Record<string, Token> = {
  "+": { kind: "+" },
  "-": { kind: "-" },
  "*": { kind: "*" },
  "/": { kind: "/" },
  "(": { kind: "(" },
  ")": { kind: ")" },
};

/**
 * Splits an expression into tokens, or returns `null` for a character
 * outside digits, `+ - * /`, `.`, `(`, `)`, and whitespace, or for a
 * digit/dot run that is not a single valid float literal (e.g. `1.2.3`).
 */
function tokenize(input: string): Token[] | null {
  const tokens: Token[] = [];
  let i = 0;
  while (i < input.length) {
    const c = input[i];
    if (c === " " || c === "\t" || c === "\n" || c === "\r") {
      i++;
      continue;
    }
    const single = SINGLE_CHAR_TOKENS[c];
    if (single) {
      tokens.push(single);
      i++;
      continue;
    }
    if (c === "." || (c >= "0" && c <= "9")) {
      // Greedily consume the digit/decimal-point run, then validate it as
      // exactly one float literal (at most one `.`, at least one digit), the
      // same shape backend/src/expr.rs accepts.
      const start = i;
      while (i < input.length && (input[i] === "." || (input[i] >= "0" && input[i] <= "9"))) {
        i++;
      }
      const literal = input.slice(start, i);
      if (!/^\d*\.?\d*$/.test(literal) || !/\d/.test(literal)) return null;
      tokens.push({ kind: "number", value: Number(literal) });
      continue;
    }
    return null;
  }
  return tokens;
}

/**
 * Recursive-descent parser/evaluator over the token stream. Evaluation
 * happens during the parse, so there is no separate AST: the only result
 * ever needed is a single number.
 */
class Parser {
  private pos = 0;
  private readonly tokens: Token[];

  constructor(tokens: Token[]) {
    this.tokens = tokens;
  }

  isAtEnd(): boolean {
    return this.pos >= this.tokens.length;
  }

  private peek(): Token | undefined {
    return this.tokens[this.pos];
  }

  // expr := term (('+' | '-') term)*
  parseExpr(): number | null {
    let acc = this.parseTerm();
    if (acc === null) return null;
    for (;;) {
      const op = this.peek();
      if (op?.kind === "+" || op?.kind === "-") {
        this.pos++;
        const rhs = this.parseTerm();
        if (rhs === null) return null;
        acc = op.kind === "+" ? acc + rhs : acc - rhs;
      } else {
        return acc;
      }
    }
  }

  // term := unary (('*' | '/') unary)*
  private parseTerm(): number | null {
    let acc = this.parseUnary();
    if (acc === null) return null;
    for (;;) {
      const op = this.peek();
      if (op?.kind === "*" || op?.kind === "/") {
        this.pos++;
        const rhs = this.parseUnary();
        if (rhs === null) return null;
        // Division by zero produces Infinity/NaN here; evalMath's final
        // Number.isFinite check turns it into the contracted NaN result,
        // so no separate error path is needed.
        acc = op.kind === "*" ? acc * rhs : acc / rhs;
      } else {
        return acc;
      }
    }
  }

  // unary := ('+' | '-') unary | atom
  private parseUnary(): number | null {
    const tok = this.peek();
    if (tok?.kind === "+") {
      this.pos++;
      return this.parseUnary();
    }
    if (tok?.kind === "-") {
      this.pos++;
      const value = this.parseUnary();
      return value === null ? null : -value;
    }
    return this.parseAtom();
  }

  // atom := NUMBER | '(' expr ')'
  private parseAtom(): number | null {
    const tok = this.tokens[this.pos++];
    if (tok?.kind === "number") return tok.value;
    if (tok?.kind === "(") {
      const value = this.parseExpr();
      if (value === null) return null;
      return this.tokens[this.pos++]?.kind === ")" ? value : null;
    }
    return null;
  }
}

/**
 * Evaluates a simple arithmetic expression (digits, `+ - * /`, parentheses,
 * decimal points, and whitespace only) and returns the numeric result, or `NaN` for
 * any invalid input, including division by zero, an empty string, and any
 * character outside that allowed set. A numeric `input` is returned as-is.
 */
export function evalMath(input: string | number): number {
  if (typeof input === "number") return input;
  const trimmed = String(input).trim();
  if (!trimmed) return NaN;
  const tokens = tokenize(trimmed);
  if (tokens === null || tokens.length === 0) return NaN;
  const parser = new Parser(tokens);
  const value = parser.parseExpr();
  if (value === null || !parser.isAtEnd()) return NaN;
  return Number.isFinite(value) ? value : NaN;
}
