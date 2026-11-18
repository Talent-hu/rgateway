/**
 * Prometheus 文本解析器（exposition format）。
 *
 * 把 `/metrics` 返回的纯文本解析为 `{name, labels, value}` 样本数组。
 * 解析是容错的：注释、HELP/TYPE 行、畸形行一律跳过，绝不抛出异常。
 */

/** 单条指标样本：指标名 + 标签集 + 数值 */
export interface MetricSample {
  name: string;
  labels: Record<string, string>;
  value: number;
}

/** 指标名：字母/下划线/冒号开头，后接字母数字下划线冒号 */
const NAME_RE = /^[a-zA-Z_:][a-zA-Z0-9_:]*/;
/** 标签名：字母下划线开头，后接字母数字下划线 */
const LABEL_KEY_RE = /^[a-zA-Z_][a-zA-Z0-9_]*/;

/**
 * 解析 Prometheus 文本为样本数组。
 *
 * @param text /metrics 返回的原始文本
 * @returns 样本数组（按出现顺序）；畸形行被跳过
 */
export function parsePrometheusText(text: string): MetricSample[] {
  const samples: MetricSample[] = [];
  for (const rawLine of text.split('\n')) {
    const line = rawLine.trim();
    if (line === '' || line.startsWith('#')) {
      continue;
    }
    const sample = parseLine(line);
    if (sample !== null) {
      samples.push(sample);
    }
  }
  return samples;
}

/** 解析单行样本；畸形行返回 null（跳过） */
function parseLine(line: string): MetricSample | null {
  const nameMatch = NAME_RE.exec(line);
  if (nameMatch === null) {
    return null;
  }
  const name = nameMatch[0];
  let rest = line.slice(name.length);

  let labels: Record<string, string> = {};
  if (rest.startsWith('{')) {
    const close = findClosingBrace(rest);
    if (close < 0) {
      return null;
    }
    const parsed = parseLabelSet(rest.slice(1, close));
    if (parsed === null) {
      return null;
    }
    labels = parsed;
    rest = rest.slice(close + 1);
  }

  const token = rest.trim().split(/\s+/)[0];
  if (token === undefined || token === '') {
    return null;
  }
  const value = parseSampleValue(token);
  if (value === null) {
    return null;
  }
  return { name, labels, value };
}

/** 在 `rest`（以 `{` 开头）中定位配对的右花括号；找不到返回 -1 */
function findClosingBrace(rest: string): number {
  let inQuote = false;
  for (let i = 1; i < rest.length; i += 1) {
    const ch = rest[i];
    if (inQuote) {
      if (ch === '\\') {
        i += 1; // 跳过被转义字符
      } else if (ch === '"') {
        inQuote = false;
      }
    } else if (ch === '"') {
      inQuote = true;
    } else if (ch === '}') {
      return i;
    }
  }
  return -1;
}

/** 解析花括号内的标签段 `k="v",k2="v2"`；畸形返回 null，空段返回空对象 */
function parseLabelSet(segment: string): Record<string, string> | null {
  const labels: Record<string, string> = {};
  const s = segment.trim();
  if (s === '') {
    return labels;
  }
  let i = 0;
  while (i < s.length) {
    const keyMatch = LABEL_KEY_RE.exec(s.slice(i));
    if (keyMatch === null) {
      return null;
    }
    const key = keyMatch[0];
    i += key.length;
    if (s[i] !== '=') {
      return null;
    }
    i += 1;
    const parsed = parseQuotedValue(s, i);
    if (parsed === null) {
      return null;
    }
    labels[key] = parsed.value;
    i = parsed.next;
    while (i < s.length && s[i] === ' ') {
      i += 1;
    }
    if (i >= s.length) {
      break;
    }
    if (s[i] !== ',') {
      return null;
    }
    i += 1;
    while (i < s.length && s[i] === ' ') {
      i += 1;
    }
    if (i >= s.length) {
      break; // 容忍尾逗号
    }
  }
  return labels;
}

/** 合法转义表（exposition format 只定义这三种）；其余转义视为畸形 */
const LABEL_ESCAPES: Record<string, string> = {
  n: '\n',
  '"': '"',
  '\\': '\\',
};

/** 解析双引号标签值（支持 `\"`、`\\`、`\n` 转义）；畸形返回 null */
function parseQuotedValue(
  s: string,
  start: number
): { value: string; next: number } | null {
  if (s[start] !== '"') {
    return null;
  }
  let value = '';
  let i = start + 1;
  while (i < s.length) {
    const ch = s[i];
    if (ch === '\\') {
      const next = s[i + 1];
      const mapped = next === undefined ? undefined : LABEL_ESCAPES[next];
      if (mapped === undefined) {
        return null; // 未知转义或悬空反斜杠：整行按畸形丢弃
      }
      value += mapped;
      i += 2;
    } else if (ch === '"') {
      return { value, next: i + 1 };
    } else {
      value += ch;
      i += 1;
    }
  }
  return null; // 引号未闭合
}

/** 解析样本数值（含 +Inf / -Inf / NaN）；无法解析返回 null */
function parseSampleValue(token: string): number | null {
  const lower = token.toLowerCase();
  if (lower === '+inf' || lower === 'inf') {
    return Number.POSITIVE_INFINITY;
  }
  if (lower === '-inf') {
    return Number.NEGATIVE_INFINITY;
  }
  if (lower === 'nan') {
    return Number.NaN;
  }
  const n = Number(token);
  return Number.isFinite(n) ? n : null;
}
