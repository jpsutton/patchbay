/*---------------------------------------------------------------------------------------------
 *  Adapted from VS Code's terminal type-ahead addon
 *  (src/vs/workbench/contrib/terminalContrib/typeAhead/browser/terminalTypeAheadAddon.ts).
 *  Copyright (c) Microsoft Corporation. Licensed under the MIT License; see
 *  https://github.com/microsoft/vscode/blob/main/LICENSE.txt
 *--------------------------------------------------------------------------------------------*/

// Local echo for terminals whose shell is far away, in the style of mosh: typed characters,
// backspaces and cursor moves are drawn at once (dimmed), then checked against what the shell
// echoes. A match swaps in the real output; a miss rolls the guesses back and stops guessing
// until the line settles. Guesses are shown only once measured echo latency passes a threshold.
//
// Changes from VS Code: no DI, config or telemetry; the host feeds shell output through
// `beforeServerInput`; the guess style is fixed to dim; `StringReader.eatStr` and
// `arrayHasPrefixAt` compare at the right offsets.

import type { IBuffer, IBufferCell, IDisposable, ITerminalAddon, Terminal } from "@xterm/xterm";

const VT = {
  Esc: "\x1b",
  Csi: "\x1b[",
  ShowCursor: "\x1b[?25h",
  HideCursor: "\x1b[?25l",
  DeleteChar: "\x1b[X",
  DeleteRestOfLine: "\x1b[K",
} as const;

const CSI_STYLE_RE = /^\x1b\[[0-9;]*m/;
const CSI_MOVE_RE = /^\x1b\[?([0-9]*)(;[35])?O?([DC])/;
const NOT_WORD_RE = /[^a-z0-9]/i;

const STATS_BUFFER_SIZE = 24;
const STATS_MIN_SAMPLES_TO_TURN_ON = 5;
const STATS_MIN_ACCURACY_TO_TURN_ON = 0.3;
/** If latency is less than `threshold * this`, turn off. */
const STATS_TOGGLE_OFF_THRESHOLD = 0.5;

/** Full-screen or pager programs, matched against the terminal title, where guessing is off. */
const EXCLUDE_PROGRAM_RE = /\b(vim|vi|nano|tmux)\b/i;

/**
 * Codes passed straight through instead of to the prediction engine:
 * - Hide/show cursor (DECTCEM): local echo is wrapped in these.
 * - Device Status Report: these make xterm reply, which could double-report.
 */
const PREDICTION_OMIT_RE = /^(\x1b\[(\??25[hl]|\??[0-9;]+n))+/;

type SingleOrMany<T> = T | T[];
const isNumber = (v: unknown): v is number => typeof v === "number";

/** Cell attributes, as on `IBufferCell` and on xterm's private current-attribute data. */
type XtermAttributes = Pick<
  IBufferCell,
  | "isAttributeDefault"
  | "isBold"
  | "isDim"
  | "isItalic"
  | "isUnderline"
  | "isBlink"
  | "isInverse"
  | "isInvisible"
  | "isFgRGB"
  | "isFgPalette"
  | "isFgDefault"
  | "isBgRGB"
  | "isBgPalette"
  | "isBgDefault"
  | "getFgColor"
  | "getBgColor"
>;

/** The attributes xterm will apply to the next character it prints (private API). */
const currentAttributes = (terminal: Terminal): XtermAttributes =>
  (terminal as unknown as { _core: { _inputHandler: { _curAttrData: XtermAttributes } } })._core._inputHandler
    ._curAttrData;

class Emitter<T> {
  private listeners = new Set<(e: T) => void>();
  readonly event = (fn: (e: T) => void): IDisposable => {
    this.listeners.add(fn);
    return { dispose: () => this.listeners.delete(fn) };
  };
  fire(e: T) {
    for (const fn of [...this.listeners]) fn(e);
  }
  dispose() {
    this.listeners.clear();
  }
}

const enum CursorMoveDirection {
  Back = "D",
  Forwards = "C",
}

interface ICoordinate {
  x: number;
  y: number;
  baseY: number;
}

class Cursor implements ICoordinate {
  private _x = 0;
  private _y = 1;
  private _baseY = 1;

  get x() {
    return this._x;
  }

  get y() {
    return this._y;
  }

  get baseY() {
    return this._baseY;
  }

  get coordinate(): ICoordinate {
    return { x: this._x, y: this._y, baseY: this._baseY };
  }

  constructor(
    readonly rows: number,
    readonly cols: number,
    private readonly _buffer: IBuffer,
  ) {
    this._x = _buffer.cursorX;
    this._y = _buffer.cursorY;
    this._baseY = _buffer.baseY;
  }

  getLine() {
    return this._buffer.getLine(this._y + this._baseY);
  }

  getCell(loadInto?: IBufferCell) {
    return this.getLine()?.getCell(this._x, loadInto);
  }

  moveTo(coordinate: ICoordinate) {
    this._x = coordinate.x;
    this._y = coordinate.y + coordinate.baseY - this._baseY;
    return this.moveInstruction();
  }

  clone() {
    const c = new Cursor(this.rows, this.cols, this._buffer);
    c.moveTo(this);
    return c;
  }

  move(x: number, y: number) {
    this._x = x;
    this._y = y;
    return this.moveInstruction();
  }

  shift(x: number = 0, y: number = 0) {
    this._x += x;
    this._y += y;
    return this.moveInstruction();
  }

  moveInstruction() {
    if (this._y >= this.rows) {
      this._baseY += this._y - (this.rows - 1);
      this._y = this.rows - 1;
    } else if (this._y < 0) {
      this._baseY -= this._y;
      this._y = 0;
    }

    return `${VT.Csi}${this._y + 1};${this._x + 1}H`;
  }
}

const moveToWordBoundary = (_b: IBuffer, cursor: Cursor, direction: -1 | 1) => {
  let ateLeadingWhitespace = false;
  if (direction < 0) {
    cursor.shift(-1);
  }

  let cell: IBufferCell | undefined;
  while (cursor.x >= 0) {
    cell = cursor.getCell(cell);
    if (!cell?.getCode()) {
      return;
    }

    const chars = cell.getChars();
    if (NOT_WORD_RE.test(chars)) {
      if (ateLeadingWhitespace) {
        break;
      }
    } else {
      ateLeadingWhitespace = true;
    }

    cursor.shift(direction);
  }

  if (direction < 0) {
    cursor.shift(1); // place the cursor after the whitespace starting the word
  }
};

const enum MatchResult {
  /** matched successfully */
  Success,
  /** failed to match */
  Failure,
  /** buffer data, it might match once more data comes in */
  Buffer,
}

interface IPrediction {
  /**
   * Whether applying this prediction can modify the style attributes of the terminal. If so
   * the cursor style needs resetting when it's rolled back.
   */
  readonly affectsStyle?: boolean;

  /** If false, the prediction is not cleared when no input comes from the server. */
  readonly clearAfterTimeout?: boolean;

  /** Returns a sequence that applies the prediction at `cursor`, advancing it. */
  apply(buffer: IBuffer, cursor: Cursor): string;

  /** Returns a sequence that rolls back a previous `apply()`. */
  rollback(cursor: Cursor): string;

  /** Called when the prediction is correct. */
  rollForwards(cursor: Cursor, withInput: string): string;

  /** Whether the shell output in `input` is what this prediction expected. */
  matches(input: StringReader, lookBehind?: IPrediction): MatchResult;
}

class StringReader {
  index = 0;

  get remaining() {
    return this._input.length - this.index;
  }

  get eof() {
    return this.index === this._input.length;
  }

  get rest() {
    return this._input.slice(this.index);
  }

  constructor(private readonly _input: string) {}

  /** Advances the reader and returns the character if it matches. */
  eatChar(char: string) {
    if (this._input[this.index] !== char) {
      return;
    }

    this.index++;
    return char;
  }

  /** Advances the reader and returns the string if it matches. */
  eatStr(substr: string) {
    if (this._input.slice(this.index, this.index + substr.length) !== substr) {
      return;
    }

    this.index += substr.length;
    return substr;
  }

  /**
   * Matches and eats the substring character by character. If EOF comes before the substring
   * is consumed, it buffers. The index does not move if it's not a match.
   */
  eatGradually(substr: string): MatchResult {
    const prevIndex = this.index;
    for (let i = 0; i < substr.length; i++) {
      if (i > 0 && this.eof) {
        return MatchResult.Buffer;
      }

      if (!this.eatChar(substr[i])) {
        this.index = prevIndex;
        return MatchResult.Failure;
      }
    }

    return MatchResult.Success;
  }

  /** Advances the reader and returns the regex match if it matches. */
  eatRe(re: RegExp) {
    const match = re.exec(this._input.slice(this.index));
    if (!match) {
      return;
    }

    this.index += match[0].length;
    return match;
  }

  /** Advances the reader and returns the character code if it is in [min, max). */
  eatCharCode(min = 0, max = min + 1) {
    const code = this._input.charCodeAt(this.index);
    if (code < min || code >= max) {
      return undefined;
    }

    this.index++;
    return code;
  }
}

/** Prediction that never tests true. Always discards predictions made after it. */
class HardBoundary implements IPrediction {
  readonly clearAfterTimeout = false;

  apply() {
    return "";
  }

  rollback() {
    return "";
  }

  rollForwards() {
    return "";
  }

  matches() {
    return MatchResult.Failure;
  }
}

/** Wraps another prediction. Does not apply it, but passes through its `matches`. */
class TentativeBoundary implements IPrediction {
  private _appliedCursor?: Cursor;

  constructor(readonly inner: IPrediction) {}

  apply(buffer: IBuffer, cursor: Cursor) {
    this._appliedCursor = cursor.clone();
    this.inner.apply(buffer, this._appliedCursor);
    return "";
  }

  rollback(cursor: Cursor) {
    this.inner.rollback(cursor.clone());
    return "";
  }

  rollForwards(cursor: Cursor, withInput: string) {
    if (this._appliedCursor) {
      cursor.moveTo(this._appliedCursor);
    }

    return withInput;
  }

  matches(input: StringReader) {
    return this.inner.matches(input);
  }
}

const isTentativeCharacterPrediction = (p: unknown): p is TentativeBoundary & { inner: CharacterPrediction } =>
  p instanceof TentativeBoundary && p.inner instanceof CharacterPrediction;

/** Prediction for a single printable character. */
class CharacterPrediction implements IPrediction {
  readonly affectsStyle = true;

  appliedAt?: {
    pos: ICoordinate;
    oldAttributes: string;
    oldChar: string;
  };

  constructor(
    private readonly _style: TypeAheadStyle,
    private readonly _char: string,
  ) {}

  apply(_: IBuffer, cursor: Cursor) {
    const cell = cursor.getCell();
    this.appliedAt = cell
      ? { pos: cursor.coordinate, oldAttributes: attributesToSeq(cell), oldChar: cell.getChars() }
      : { pos: cursor.coordinate, oldAttributes: "", oldChar: "" };

    cursor.shift(1);

    return this._style.apply + this._char + this._style.undo;
  }

  rollback(cursor: Cursor) {
    if (!this.appliedAt) {
      return ""; // not applied
    }

    const { oldAttributes, oldChar, pos } = this.appliedAt;
    return cursor.moveTo(pos) + (oldChar ? `${oldAttributes}${oldChar}${cursor.moveTo(pos)}` : VT.DeleteChar);
  }

  rollForwards(cursor: Cursor, input: string) {
    if (!this.appliedAt) {
      return ""; // not applied
    }

    return cursor.clone().moveTo(this.appliedAt.pos) + input;
  }

  matches(input: StringReader, lookBehind?: IPrediction) {
    const startIndex = input.index;

    // skip any styling CSI before checking the char
    while (input.eatRe(CSI_STYLE_RE)) {
      /* eat */
    }

    if (input.eof) {
      return MatchResult.Buffer;
    }

    if (input.eatChar(this._char)) {
      return MatchResult.Success;
    }

    if (lookBehind instanceof CharacterPrediction) {
      // zsh sometimes redraws the previous character too (vscode#112842)
      const sillyZshOutcome = input.eatGradually(`\b${lookBehind._char}${this._char}`);
      if (sillyZshOutcome !== MatchResult.Failure) {
        return sillyZshOutcome;
      }
    }

    input.index = startIndex;
    return MatchResult.Failure;
  }
}

class BackspacePrediction implements IPrediction {
  protected _appliedAt?: {
    pos: ICoordinate;
    oldAttributes: string;
    oldChar: string;
    isLastChar: boolean;
  };

  constructor(private readonly _terminal: Terminal) {}

  apply(_: IBuffer, cursor: Cursor) {
    // at eol if everything to the right is whitespace (zsh emits a "clear line" code then)
    const isLastChar = !cursor.getLine()?.translateToString(undefined, cursor.x).trim();
    const pos = cursor.coordinate;
    const move = cursor.shift(-1);
    const cell = cursor.getCell();
    this._appliedAt = cell
      ? { isLastChar, pos, oldAttributes: attributesToSeq(cell), oldChar: cell.getChars() }
      : { isLastChar, pos, oldAttributes: "", oldChar: "" };

    return move + VT.DeleteChar;
  }

  rollback(cursor: Cursor) {
    if (!this._appliedAt) {
      return ""; // not applied
    }

    const { oldAttributes, oldChar, pos } = this._appliedAt;
    if (!oldChar) {
      return cursor.moveTo(pos) + VT.DeleteChar;
    }

    return oldAttributes + oldChar + cursor.moveTo(pos) + attributesToSeq(currentAttributes(this._terminal));
  }

  rollForwards() {
    return "";
  }

  matches(input: StringReader) {
    if (this._appliedAt?.isLastChar) {
      const r1 = input.eatGradually(`\b${VT.Csi}K`);
      if (r1 !== MatchResult.Failure) {
        return r1;
      }

      const r2 = input.eatGradually(`\b \b`);
      if (r2 !== MatchResult.Failure) {
        return r2;
      }
    }

    return MatchResult.Failure;
  }
}

class NewlinePrediction implements IPrediction {
  protected _prevPosition?: ICoordinate;

  apply(_: IBuffer, cursor: Cursor) {
    this._prevPosition = cursor.coordinate;
    cursor.move(0, cursor.y + 1);
    return "\r\n";
  }

  rollback(cursor: Cursor) {
    return this._prevPosition ? cursor.moveTo(this._prevPosition) : "";
  }

  rollForwards() {
    return ""; // does not need to rewrite
  }

  matches(input: StringReader) {
    return input.eatGradually("\r\n");
  }
}

/**
 * Prediction when the cursor reaches the end of the line. Like a newline, but shells handle
 * it slightly differently.
 */
class LinewrapPrediction extends NewlinePrediction implements IPrediction {
  override apply(_: IBuffer, cursor: Cursor) {
    this._prevPosition = cursor.coordinate;
    cursor.move(0, cursor.y + 1);
    return " \r";
  }

  override matches(input: StringReader) {
    // bash and zsh add a space which wraps in the terminal, then a CR
    const r = input.eatGradually(" \r");
    if (r !== MatchResult.Failure) {
      // zsh also clears the line after wrapping, to be safe: eat it
      const r2 = input.eatGradually(VT.DeleteRestOfLine);
      return r2 === MatchResult.Buffer ? MatchResult.Buffer : r;
    }

    return input.eatGradually("\r\n");
  }
}

class CursorMovePrediction implements IPrediction {
  private _applied?: {
    rollForward: string;
    prevPosition: number;
    prevAttrs: string;
    amount: number;
  };

  constructor(
    private readonly _direction: CursorMoveDirection,
    private readonly _moveByWords: boolean,
    private readonly _amount: number,
  ) {}

  apply(buffer: IBuffer, cursor: Cursor) {
    const prevPosition = cursor.x;
    const currentCell = cursor.getCell();
    const prevAttrs = currentCell ? attributesToSeq(currentCell) : "";

    const { _amount: amount, _direction: direction, _moveByWords: moveByWords } = this;
    const delta = direction === CursorMoveDirection.Back ? -1 : 1;

    const target = cursor.clone();
    if (moveByWords) {
      for (let i = 0; i < amount; i++) {
        moveToWordBoundary(buffer, target, delta);
      }
    } else {
      target.shift(delta * amount);
    }

    this._applied = {
      amount: Math.abs(cursor.x - target.x),
      prevPosition,
      prevAttrs,
      rollForward: cursor.moveTo(target),
    };

    return this._applied.rollForward;
  }

  rollback(cursor: Cursor) {
    if (!this._applied) {
      return "";
    }

    return cursor.move(this._applied.prevPosition, cursor.y) + this._applied.prevAttrs;
  }

  rollForwards() {
    return ""; // does not need to rewrite
  }

  matches(input: StringReader) {
    if (!this._applied) {
      return MatchResult.Failure;
    }

    const direction = this._direction;
    const { amount, rollForward } = this._applied;

    // The argument can be omitted to move one character. Not eatGradually() here or below:
    // moves that don't go as far as the cursor would buffer forever.
    if (input.eatStr(`${VT.Csi}${direction}`.repeat(amount))) {
      return MatchResult.Success;
    }

    // \b is the same as moving one character back
    if (direction === CursorMoveDirection.Back) {
      if (input.eatStr(`\b`.repeat(amount))) {
        return MatchResult.Success;
      }
    }

    // the cursor position may be set absolutely
    if (rollForward) {
      const r = input.eatGradually(rollForward);
      if (r !== MatchResult.Failure) {
        return r;
      }
    }

    // a relative move in the direction
    return input.eatGradually(`${VT.Csi}${amount}${direction}`);
  }
}

class PredictionStats {
  private readonly _stats: [latency: number, correct: boolean][] = [];
  private _index = 0;
  private readonly _addedAtTime = new WeakMap<IPrediction, number>();
  private readonly _changeEmitter = new Emitter<void>();
  readonly onChange = this._changeEmitter.event;
  private readonly _subs: IDisposable[];

  /** Share (0-1) of predictions that were right. */
  get accuracy() {
    let correctCount = 0;
    for (const [, correct] of this._stats) {
      if (correct) {
        correctCount++;
      }
    }

    return correctCount / (this._stats.length || 1);
  }

  get sampleSize() {
    return this._stats.length;
  }

  /** Latency stats of successful predictions. */
  get latency() {
    const latencies = this._stats
      .filter(([, correct]) => correct)
      .map(([s]) => s)
      .sort((a, b) => a - b);

    return {
      count: latencies.length,
      min: latencies[0],
      median: latencies[Math.floor(latencies.length / 2)],
      max: latencies[latencies.length - 1],
    };
  }

  /** The highest observed latency. */
  get maxLatency() {
    let max = -Infinity;
    for (const [latency, correct] of this._stats) {
      if (correct) {
        max = Math.max(latency, max);
      }
    }

    return max;
  }

  constructor(timeline: PredictionTimeline) {
    this._subs = [
      timeline.onPredictionAdded((p) => this._addedAtTime.set(p, Date.now())),
      timeline.onPredictionSucceeded((p) => this._pushStat(true, p)),
      timeline.onPredictionFailed((p) => this._pushStat(false, p)),
    ];
  }

  dispose() {
    this._subs.forEach((s) => s.dispose());
    this._changeEmitter.dispose();
  }

  private _pushStat(correct: boolean, prediction: IPrediction) {
    const started = this._addedAtTime.get(prediction)!;
    this._stats[this._index] = [Date.now() - started, correct];
    this._index = (this._index + 1) % STATS_BUFFER_SIZE;
    this._changeEmitter.fire();
  }
}

class PredictionTimeline {
  /** Expected queue of events. Only predictions for the lowest generation are on screen. */
  private _expected: { gen: number; p: IPrediction }[] = [];

  private _currentGen = 0;

  /**
   * Where the cursor is on screen (or will be once pending writes flush). Kept outside the
   * buffer since it can be ahead when typing quickly.
   */
  private _physicalCursor: Cursor | undefined;

  /**
   * Cursor position counting all predictions, applied or not. A new prediction is applied
   * here.
   */
  private _tentativeCursor: Cursor | undefined;

  /** Shell output held back because it might still match; prepended to the next output. */
  private _inputBuffer?: string;

  /**
   * Whether predictions are drawn. When false they are still tracked, for latency stats, but
   * output is never changed.
   */
  private _showPredictions = false;

  /** The last prediction that matched. */
  private _lookBehind?: IPrediction;

  private readonly _addedEmitter = new Emitter<IPrediction>();
  readonly onPredictionAdded = this._addedEmitter.event;
  private readonly _failedEmitter = new Emitter<IPrediction>();
  readonly onPredictionFailed = this._failedEmitter.event;
  private readonly _succeededEmitter = new Emitter<IPrediction>();
  readonly onPredictionSucceeded = this._succeededEmitter.event;

  private get _currentGenerationPredictions() {
    return this._expected.filter(({ gen }) => gen === this._expected[0].gen).map(({ p }) => p);
  }

  get isShowingPredictions() {
    return this._showPredictions;
  }

  get length() {
    return this._expected.length;
  }

  constructor(
    readonly terminal: Terminal,
    private readonly _style: TypeAheadStyle,
  ) {}

  dispose() {
    this._addedEmitter.dispose();
    this._failedEmitter.dispose();
    this._succeededEmitter.dispose();
  }

  setShowPredictions(show: boolean) {
    if (show === this._showPredictions) {
      return;
    }

    this._showPredictions = show;

    const buffer = this._getActiveBuffer();
    if (!buffer) {
      return;
    }

    const toApply = this._currentGenerationPredictions;
    if (show) {
      this.clearCursor();
      this._style.expectIncomingStyle(toApply.reduce((count, p) => (p.affectsStyle ? count + 1 : count), 0));
      this.terminal.write(toApply.map((p) => p.apply(buffer, this.physicalCursor(buffer))).join(""));
    } else {
      this.terminal.write(
        toApply
          .reverse()
          .map((p) => p.rollback(this.physicalCursor(buffer)))
          .join(""),
      );
    }
  }

  /** Undoes any predictions on screen and resets expectations. */
  undoAllPredictions() {
    const buffer = this._getActiveBuffer();
    if (this._showPredictions && buffer) {
      this.terminal.write(
        this._currentGenerationPredictions
          .reverse()
          .map((p) => p.rollback(this.physicalCursor(buffer)))
          .join(""),
      );
    }

    this._expected = [];
  }

  /** Takes shell output on its way to the terminal and returns what to write instead. */
  beforeServerInput(input: string): string {
    const originalInput = input;
    if (this._inputBuffer) {
      input = this._inputBuffer + input;
      this._inputBuffer = undefined;
    }

    if (!this._expected.length) {
      this._clearPredictionState();
      return input;
    }

    const buffer = this._getActiveBuffer();
    if (!buffer) {
      this._clearPredictionState();
      return input;
    }

    let output = "";

    const reader = new StringReader(input);
    const startingGen = this._expected[0].gen;
    const emitPredictionOmitted = () => {
      const omit = reader.eatRe(PREDICTION_OMIT_RE);
      if (omit) {
        output += omit[0];
      }
    };

    ReadLoop: while (this._expected.length && reader.remaining > 0) {
      emitPredictionOmitted();

      const { p: prediction, gen } = this._expected[0];
      const cursor = this.physicalCursor(buffer);
      const beforeTestReaderIndex = reader.index;
      switch (prediction.matches(reader, this._lookBehind)) {
        case MatchResult.Success: {
          // The output is what the next prediction expected: undo the prediction and write
          // the real output.
          const eaten = input.slice(beforeTestReaderIndex, reader.index);
          if (gen === startingGen) {
            output += prediction.rollForwards?.(cursor, eaten);
          } else {
            prediction.apply(buffer, this.physicalCursor(buffer)); // move the cursor for later applies
            output += eaten;
          }

          this._succeededEmitter.fire(prediction);
          this._lookBehind = prediction;
          this._expected.shift();
          break;
        }
        case MatchResult.Buffer:
          // Hold the rest back; what was read so far goes out as normal.
          this._inputBuffer = input.slice(beforeTestReaderIndex);
          reader.index = input.length;
          break ReadLoop;
        case MatchResult.Failure: {
          // Roll back everything left in this generation and drop the predictions, since
          // they no longer hold.
          const rollback = this._expected.filter((p) => p.gen === startingGen).reverse();
          output += rollback.map(({ p }) => p.rollback(this.physicalCursor(buffer))).join("");
          if (rollback.some((r) => r.p.affectsStyle)) {
            // Predictions restore the style they change, so the current style is safe to read.
            output += attributesToSeq(currentAttributes(this.terminal));
          }
          this._clearPredictionState();
          this._failedEmitter.fire(prediction);
          break ReadLoop;
        }
      }
    }

    emitPredictionOmitted();

    // Extra data (such as a command's output) resets the cursor.
    if (!reader.eof) {
      output += reader.rest;
      this._clearPredictionState();
    }

    // Past a generation boundary, apply the next generation's predictions.
    if (this._expected.length && startingGen !== this._expected[0].gen) {
      for (const { p, gen } of this._expected) {
        if (gen !== this._expected[0].gen) {
          break;
        }
        if (p.affectsStyle) {
          this._style.expectIncomingStyle();
        }

        output += p.apply(buffer, this.physicalCursor(buffer));
      }
    }

    if (!this._showPredictions) {
      return originalInput;
    }

    if (output.length === 0 || output === input) {
      return output;
    }

    if (this._physicalCursor) {
      output += this._physicalCursor.moveInstruction();
    }

    // keep the cursor from flickering while typing
    return VT.HideCursor + output + VT.ShowCursor;
  }

  /** Clears expected predictions and stored state, when the shell sends something unknown. */
  private _clearPredictionState() {
    this._expected = [];
    this.clearCursor();
    this._lookBehind = undefined;
  }

  /** Appends a type-ahead prediction. */
  addPrediction(buffer: IBuffer, prediction: IPrediction) {
    this._expected.push({ gen: this._currentGen, p: prediction });
    this._addedEmitter.fire(prediction);

    if (this._currentGen !== this._expected[0].gen) {
      prediction.apply(buffer, this.tentativeCursor(buffer));
      return false;
    }

    const text = prediction.apply(buffer, this.physicalCursor(buffer));
    this._tentativeCursor = undefined; // the next read gets or clones the physical cursor

    if (this._showPredictions && text) {
      if (prediction.affectsStyle) {
        this._style.expectIncomingStyle();
      }
      this.terminal.write(text);
    }

    return true;
  }

  /**
   * Appends a prediction followed by a boundary. Predictions added after it are shown only
   * once it matches shell output.
   */
  addBoundary(): void;
  addBoundary(buffer: IBuffer, prediction: IPrediction): boolean;
  addBoundary(buffer?: IBuffer, prediction?: IPrediction) {
    let applied = false;
    if (buffer && prediction) {
      // Applied so that it's matched against, but wrapped in a TentativeBoundary so it doesn't
      // move the physical cursor. Then applied to the tentative cursor specifically.
      applied = this.addPrediction(buffer, new TentativeBoundary(prediction));
      prediction.apply(buffer, this.tentativeCursor(buffer));
    }
    this._currentGen++;
    return applied;
  }

  /** The last prediction added. */
  peekEnd(): IPrediction | undefined {
    return this._expected[this._expected.length - 1]?.p;
  }

  /** The first pending prediction. */
  peekStart(): IPrediction | undefined {
    return this._expected[0]?.p;
  }

  /** Current position of the cursor in the terminal. */
  physicalCursor(buffer: IBuffer) {
    if (!this._physicalCursor) {
      this._physicalCursor = new Cursor(this.terminal.rows, this.terminal.cols, buffer);
    }

    return this._physicalCursor;
  }

  /** Cursor position if every prediction and boundary added so far turns out right. */
  tentativeCursor(buffer: IBuffer) {
    if (!this._tentativeCursor) {
      this._tentativeCursor = this.physicalCursor(buffer).clone();
    }

    return this._tentativeCursor;
  }

  clearCursor() {
    this._physicalCursor = undefined;
    this._tentativeCursor = undefined;
  }

  private _getActiveBuffer() {
    const buffer = this.terminal.buffer.active;
    return buffer.type === "normal" ? buffer : undefined;
  }
}

/** SGR arguments that restore a cell's appearance. */
const attributesToArgs = (cell: XtermAttributes) => {
  if (cell.isAttributeDefault()) {
    return [0];
  }

  const args = [];
  if (cell.isBold()) args.push(1);
  if (cell.isDim()) args.push(2);
  if (cell.isItalic()) args.push(3);
  if (cell.isUnderline()) args.push(4);
  if (cell.isBlink()) args.push(5);
  if (cell.isInverse()) args.push(7);
  if (cell.isInvisible()) args.push(8);

  if (cell.isFgRGB()) {
    args.push(38, 2, cell.getFgColor() >>> 24, (cell.getFgColor() >>> 16) & 0xff, cell.getFgColor() & 0xff);
  }
  if (cell.isFgPalette()) args.push(38, 5, cell.getFgColor());
  if (cell.isFgDefault()) args.push(39);

  if (cell.isBgRGB()) {
    args.push(48, 2, cell.getBgColor() >>> 24, (cell.getBgColor() >>> 16) & 0xff, cell.getBgColor() & 0xff);
  }
  if (cell.isBgPalette()) args.push(48, 5, cell.getBgColor());
  if (cell.isBgDefault()) args.push(49);

  return args;
};

/** The escape sequence that restores a cell's appearance. */
const attributesToSeq = (cell: XtermAttributes) => `${VT.Csi}${attributesToArgs(cell).join(";")}m`;

/** Whether `a` holds all of `b` starting at index `ai`. */
const arrayHasPrefixAt = <T>(a: ReadonlyArray<T>, ai: number, b: ReadonlyArray<T>) => {
  if (a.length - ai < b.length) {
    return false;
  }

  for (let bi = 0; bi < b.length; bi++) {
    if (a[ai + bi] !== b[bi]) {
      return false;
    }
  }

  return true;
};

/** @see https://github.com/xtermjs/xterm.js/blob/065eb13a9d3145bea687239680ec9696d9112b8e/src/common/InputHandler.ts#L2127 */
const getColorWidth = (params: SingleOrMany<number>[], pos: number) => {
  const accu = [0, 0, -1, 0, 0, 0];
  let cSpace = 0;
  let advance = 0;

  do {
    const v = params[pos + advance];
    accu[advance + cSpace] = isNumber(v) ? v : v[0];
    if (!isNumber(v)) {
      let i = 0;
      do {
        if (accu[1] === 5) {
          cSpace = 1;
        }
        accu[advance + i + 1 + cSpace] = v[i];
      } while (++i < v.length && i + advance + 1 + cSpace < accu.length);
      break;
    }
    // exit early if the color mode is decided by semicolons
    if ((accu[1] === 5 && advance + cSpace >= 2) || (accu[1] === 2 && advance + cSpace >= 5)) {
      break;
    }
    // offset the color space slot for semicolon mode
    if (accu[1]) {
      cSpace = 1;
    }
  } while (++advance + pos < params.length && advance + cSpace < accu.length);

  return advance;
};

/** How guesses look: dim, undone with "normal intensity" unless the shell's output says otherwise. */
class TypeAheadStyle implements IDisposable {
  private static _compileArgs(args: ReadonlyArray<number>) {
    return `${VT.Csi}${args.join(";")}m`;
  }

  /**
   * Number of type-ahead style arguments still expected back. If this is 0 and a style comes
   * in, the shell really meant to change it.
   */
  private _expectedIncomingStyles = 0;
  private readonly _applyArgs: ReadonlyArray<number> = [2];
  private readonly _originalUndoArgs: ReadonlyArray<number> = [22];
  private _undoArgs: ReadonlyArray<number> = this._originalUndoArgs;

  readonly apply = TypeAheadStyle._compileArgs(this._applyArgs);
  undo = TypeAheadStyle._compileArgs(this._undoArgs);
  private _csiHandler?: IDisposable;
  private _stopTimer?: ReturnType<typeof setTimeout>;

  constructor(private readonly _terminal: Terminal) {}

  /** A style was written to the terminal: watch for it coming in. */
  expectIncomingStyle(n = 1) {
    this._expectedIncomingStyles += n * 2;
  }

  /** Starts tracking CSI changes in the terminal. */
  startTracking() {
    clearTimeout(this._stopTimer);
    this._stopTracking();
    this._expectedIncomingStyles = 0;
    this._onDidWriteSGR(attributesToArgs(currentAttributes(this._terminal)));
    this._csiHandler = this._terminal.parser.registerCsiHandler({ final: "m" }, (args) => {
      this._onDidWriteSGR(args);
      return false;
    });
  }

  /** Stops tracking terminal CSI changes after a quiet spell. */
  debounceStopTracking() {
    clearTimeout(this._stopTimer);
    this._stopTimer = setTimeout(() => this._stopTracking(), 2000);
  }

  dispose() {
    clearTimeout(this._stopTimer);
    this._stopTracking();
  }

  private _stopTracking() {
    this._csiHandler?.dispose();
    this._csiHandler = undefined;
  }

  private _onDidWriteSGR(args: SingleOrMany<number>[]) {
    const originalUndo = this._undoArgs;
    for (let i = 0; i < args.length; ) {
      const px = args[i];
      const p = isNumber(px) ? px : px[0];

      if (this._expectedIncomingStyles) {
        if (arrayHasPrefixAt(args, i, this._undoArgs)) {
          this._expectedIncomingStyles--;
          i += this._undoArgs.length;
          continue;
        }
        if (arrayHasPrefixAt(args, i, this._applyArgs)) {
          this._expectedIncomingStyles--;
          i += this._applyArgs.length;
          continue;
        }
      }

      const width = p === 38 || p === 48 || p === 58 ? getColorWidth(args, i) : 1;
      // The guess style is dim (SGR 2): undoing it must not also drop the shell's bold.
      if (p === 1) {
        this._undoArgs = [22, 1];
      } else if (p === 22 || p === 0) {
        this._undoArgs = [22];
      }

      i += width;
    }

    if (originalUndo !== this._undoArgs) {
      this.undo = TypeAheadStyle._compileArgs(this._undoArgs);
    }
  }
}

const enum CharPredictState {
  /** No characters typed on this line yet */
  Unknown,
  /** Has a pending character prediction */
  HasPendingChar,
  /** Character validated on this line */
  Validated,
}

export interface TypeAheadOptions {
  /**
   * Echo latency (ms) above which guesses are shown: 0 always shows them, a negative value
   * never does.
   */
  threshold: number;
}

/**
 * Load with `terminal.loadAddon`, then pass every chunk of shell output through
 * `beforeServerInput` before writing it to the terminal.
 */
export class TypeAheadAddon implements ITerminalAddon {
  private _style?: TypeAheadStyle;
  private _lastRow?: { y: number; startingX: number; endingX: number; charState: CharPredictState };
  private _timeline?: PredictionTimeline;
  private _terminalTitle = "";
  private _subs: IDisposable[] = [];
  private _clearTimer?: ReturnType<typeof setTimeout>;
  private _reevaluateTimer?: ReturnType<typeof setTimeout>;
  stats?: PredictionStats;

  constructor(private readonly _opts: TypeAheadOptions) {}

  activate(terminal: Terminal): void {
    const style = (this._style = new TypeAheadStyle(terminal));
    const timeline = (this._timeline = new PredictionTimeline(terminal, style));
    const stats = (this.stats = new PredictionStats(timeline));

    timeline.setShowPredictions(this._opts.threshold === 0);
    this._subs.push(
      style,
      timeline,
      stats,
      terminal.onData((e) => this._onUserData(e)),
      terminal.onTitleChange((title) => {
        this._terminalTitle = title;
        this._reevaluatePredictorState();
      }),
      terminal.onResize(() => {
        timeline.setShowPredictions(false);
        timeline.clearCursor();
        this._reevaluatePredictorState();
      }),
      timeline.onPredictionSucceeded((p) => {
        if (
          this._lastRow?.charState === CharPredictState.HasPendingChar &&
          isTentativeCharacterPrediction(p) &&
          p.inner.appliedAt &&
          p.inner.appliedAt.pos.y + p.inner.appliedAt.pos.baseY === this._lastRow.y
        ) {
          this._lastRow.charState = CharPredictState.Validated;
        }
      }),
      stats.onChange(() => {
        if (timeline.length === 0) {
          style.debounceStopTracking();
        }
        this._reevaluatePredictorState();
      }),
    );
  }

  dispose() {
    clearTimeout(this._clearTimer);
    clearTimeout(this._reevaluateTimer);
    this._subs.forEach((s) => s.dispose());
    this._subs = [];
  }

  reset() {
    this._lastRow = undefined;
  }

  /** Shell output on its way to the terminal; returns what to write instead. */
  beforeServerInput(data: string): string {
    if (!this._timeline) {
      return data;
    }
    const out = this._timeline.beforeServerInput(data);
    this._deferClearingPredictions();
    return out;
  }

  /** Drops guesses the shell never answers. */
  private _deferClearingPredictions() {
    if (!this.stats || !this._timeline) {
      return;
    }

    clearTimeout(this._clearTimer);
    this._clearTimer = undefined;
    if (this._timeline.length === 0 || this._timeline.peekStart()?.clearAfterTimeout === false) {
      return;
    }

    this._clearTimer = setTimeout(
      () => {
        this._timeline?.undoAllPredictions();
        if (this._lastRow?.charState === CharPredictState.HasPendingChar) {
          this._lastRow.charState = CharPredictState.Unknown;
        }
      },
      Math.max(500, (this.stats.maxLatency * 3) / 2),
    );
  }

  /**
   * Debounced: the state toggles only when typing pauses. Otherwise it could turn on while
   * the shell has sent data but the terminal cursor has not caught up.
   */
  private _reevaluatePredictorState() {
    clearTimeout(this._reevaluateTimer);
    this._reevaluateTimer = setTimeout(() => this._reevaluatePredictorStateNow(), 100);
  }

  private _reevaluatePredictorStateNow() {
    const stats = this.stats;
    const timeline = this._timeline;
    if (!stats || !timeline) {
      return;
    }
    const threshold = this._opts.threshold;
    if (EXCLUDE_PROGRAM_RE.test(this._terminalTitle)) {
      timeline.setShowPredictions(false);
    } else if (threshold < 0) {
      timeline.setShowPredictions(false);
    } else if (threshold === 0) {
      timeline.setShowPredictions(true);
    } else if (stats.sampleSize > STATS_MIN_SAMPLES_TO_TURN_ON && stats.accuracy > STATS_MIN_ACCURACY_TO_TURN_ON) {
      const latency = stats.latency.median;
      if (latency >= threshold) {
        timeline.setShowPredictions(true);
      } else if (latency < threshold * STATS_TOGGLE_OFF_THRESHOLD) {
        timeline.setShowPredictions(false);
      }
    }
  }

  private _onUserData(data: string): void {
    if (this._timeline?.terminal.buffer.active.type !== "normal") {
      return;
    }

    const timeline = this._timeline;
    const terminal = timeline.terminal;
    const buffer = terminal.buffer.active;

    // Programs like git log/less use the normal buffer but don't take input by default
    // (vscode#109541).
    if (buffer.cursorX === 1 && buffer.cursorY === terminal.rows - 1) {
      if (
        buffer
          .getLine(buffer.cursorY + buffer.baseY)
          ?.getCell(0)
          ?.getChars() === ":"
      ) {
        return;
      }
    }

    // Guard the prompt, so arrows and backspace can't move into it: record the lowest X at
    // which the user typed, and mark everything before it tentative.
    const actualY = buffer.baseY + buffer.cursorY;
    if (actualY !== this._lastRow?.y) {
      this._lastRow = {
        y: actualY,
        startingX: buffer.cursorX,
        endingX: buffer.cursorX,
        charState: CharPredictState.Unknown,
      };
    } else {
      this._lastRow.startingX = Math.min(this._lastRow.startingX, buffer.cursorX);
      this._lastRow.endingX = Math.max(this._lastRow.endingX, timeline.physicalCursor(buffer).x);
    }
    const lastRow = this._lastRow;

    const addLeftNavigating = (p: IPrediction) =>
      timeline.tentativeCursor(buffer).x <= lastRow.startingX
        ? timeline.addBoundary(buffer, p)
        : timeline.addPrediction(buffer, p);

    const addRightNavigating = (p: IPrediction) =>
      timeline.tentativeCursor(buffer).x >= lastRow.endingX - 1
        ? timeline.addBoundary(buffer, p)
        : timeline.addPrediction(buffer, p);

    /** @see https://github.com/xtermjs/xterm.js/blob/1913e9512c048e3cf56bb5f5df51bfff6899c184/src/common/input/Keyboard.ts */
    const reader = new StringReader(data);
    while (reader.remaining > 0) {
      if (reader.eatCharCode(127)) {
        // backspace
        const previous = timeline.peekEnd();
        if (previous && previous instanceof CharacterPrediction) {
          timeline.addBoundary();
        }

        if (timeline.tentativeCursor(buffer).x <= lastRow.startingX) {
          timeline.addBoundary(buffer, new BackspacePrediction(terminal));
        } else {
          // Backspace decrements our ability to go right.
          lastRow.endingX--;
          timeline.addPrediction(buffer, new BackspacePrediction(terminal));
        }

        continue;
      }

      if (reader.eatCharCode(32, 126)) {
        // printable
        const char = data[reader.index - 1];
        const prediction = new CharacterPrediction(this._style!, char);
        if (lastRow.charState === CharPredictState.Unknown) {
          timeline.addBoundary(buffer, prediction);
          lastRow.charState = CharPredictState.HasPendingChar;
        } else {
          timeline.addPrediction(buffer, prediction);
        }

        if (timeline.tentativeCursor(buffer).x >= terminal.cols) {
          timeline.addBoundary(buffer, new LinewrapPrediction());
        }
        continue;
      }

      const cursorMv = reader.eatRe(CSI_MOVE_RE);
      if (cursorMv) {
        const direction = cursorMv[3] as CursorMoveDirection;
        const p = new CursorMovePrediction(direction, !!cursorMv[2], Number(cursorMv[1]) || 1);
        if (direction === CursorMoveDirection.Back) {
          addLeftNavigating(p);
        } else {
          addRightNavigating(p);
        }
        continue;
      }

      if (reader.eatStr(`${VT.Esc}f`)) {
        addRightNavigating(new CursorMovePrediction(CursorMoveDirection.Forwards, true, 1));
        continue;
      }

      if (reader.eatStr(`${VT.Esc}b`)) {
        addLeftNavigating(new CursorMovePrediction(CursorMoveDirection.Back, true, 1));
        continue;
      }

      if (reader.eatChar("\r") && buffer.cursorY < terminal.rows - 1) {
        timeline.addPrediction(buffer, new NewlinePrediction());
        continue;
      }

      // something else
      timeline.addBoundary(buffer, new HardBoundary());
      break;
    }

    if (timeline.length === 1) {
      this._deferClearingPredictions();
      this._style!.startTracking();
    }
  }
}
