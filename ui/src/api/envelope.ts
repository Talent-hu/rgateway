/**
 * 统一信封解析与边界防御式取数。
 *
 * 信封形态：`{"success": bool, "data": object|null, "error": string|null}`。
 * 解析永不抛出异常：非法输入一律走 error 通道，客户端只拿到泛化文案。
 */

/** 统一结果类型：成功携带数据；失败携带面向客户端的泛化文案 */
export type ApiResult<T> = { ok: true; data: T } | { ok: false; error: string };

/** 通用失败文案（服务端未给文案时的回退） */
export const GENERIC_ERROR = '请求失败';

/** 防御式取数：非有限数字一律回退 */
export function asNumber(value: unknown, fallback = 0): number {
  if (typeof value === 'number' && Number.isFinite(value)) {
    return value;
  }
  if (typeof value === 'string' && value.trim() !== '') {
    const n = Number(value);
    if (Number.isFinite(n)) {
      return n;
    }
  }
  return fallback;
}

/** 防御式取串：非字符串一律回退 */
export function asString(value: unknown, fallback = ''): string {
  return typeof value === 'string' ? value : fallback;
}

/** 防御式取数组：非数组回退空数组 */
export function asArray(value: unknown): unknown[] {
  return Array.isArray(value) ? value : [];
}

/** 防御式取普通对象：数组/null/原始类型一律回退空对象 */
export function asRecord(value: unknown): Record<string, unknown> {
  const isPlain =
    typeof value === 'object' && value !== null && !Array.isArray(value);
  return isPlain ? (value as Record<string, unknown>) : {};
}

/**
 * 解析统一信封。
 *
 * - success !== true → error 通道（服务端文案优先，否则通用文案）
 * - success === true → data 原样带回（可以为 null）
 */
export function parseEnvelope(raw: unknown): ApiResult<unknown> {
  const rec = asRecord(raw);
  if (rec['success'] !== true) {
    const msg = asString(rec['error'], '').trim();
    return { ok: false, error: msg !== '' ? msg : GENERIC_ERROR };
  }
  return { ok: true, data: rec['data'] ?? null };
}
