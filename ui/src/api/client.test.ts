import { describe, expect, it, vi } from 'vitest';
import { ApiClient } from './client';
import type { ResetBreakerRequest } from './types';

/** 构造一个打桩 fetch，按顺序返回预设响应 */
const stubFetch = (
  responses: Array<{ status?: number; json?: unknown; text?: string }>
): typeof fetch => {
  let call = 0;
  return vi.fn(async () => {
    const spec = responses[call] ?? responses[responses.length - 1];
    call += 1;
    return {
      ok: (spec.status ?? 200) < 400,
      status: spec.status ?? 200,
      json: async () => spec.json,
      text: async () => spec.text ?? JSON.stringify(spec.json ?? {}),
    } as unknown as Response;
  }) as unknown as typeof fetch;
};

describe('ApiClient - 七个接口与信封处理', () => {
  it('health 成功解析 data', async () => {
    const client = new ApiClient({
      fetchImpl: stubFetch([{ json: { success: true, data: { status: 'ok' }, error: null } }]),
    });
    const res = await client.health();
    expect(res.ok).toBe(true);
    expect(res.ok && res.data.status).toBe('ok');
  });

  it('snapshot 防御式解析：缺字段不崩', async () => {
    const client = new ApiClient({
      fetchImpl: stubFetch([
        {
          json: {
            success: true,
            data: {
              version: 3,
              // failed_reloads / listen / route_count 缺失
              routes: [{ id: 'r1', order: 1, uri: '/api', protocol: 'http' }],
            },
            error: null,
          },
        },
      ]),
    });
    const res = await client.snapshot();
    expect(res.ok).toBe(true);
    if (res.ok) {
      expect(res.data.version).toBe(3);
      expect(res.data.failed_reloads).toBe(0);
      expect(res.data.listen).toBe('');
      expect(res.data.route_count).toBe(1);
      expect(res.data.routes).toHaveLength(1);
      expect(res.data.routes[0].id).toBe('r1');
    }
  });

  it('routes 按序解析路由摘要', async () => {
    const client = new ApiClient({
      fetchImpl: stubFetch([
        {
          json: {
            success: true,
            data: {
              routes: [
                { id: 'a', order: 0, uri: '/x', protocol: 'http' },
                { id: 'b', order: 1, uri: '/y', protocol: 'grpc' },
              ],
            },
            error: null,
          },
        },
      ]),
    });
    const res = await client.routes();
    expect(res.ok).toBe(true);
    expect(res.ok && res.data.routes.map((r) => r.id)).toEqual(['a', 'b']);
    expect(res.ok && res.data.routes[1].protocol).toBe('grpc');
  });

  it('breakers 解析逐路由逐实例状态', async () => {
    const client = new ApiClient({
      fetchImpl: stubFetch([
        {
          json: {
            success: true,
            data: {
              routes: [
                {
                  route_id: 'r1',
                  breakers: [
                    { instance_id: 'i1', state: 'closed' },
                    { instance_id: 'i2', state: 'open' },
                  ],
                },
              ],
            },
            error: null,
          },
        },
      ]),
    });
    const res = await client.breakers();
    expect(res.ok).toBe(true);
    if (res.ok) {
      expect(res.data.routes).toHaveLength(1);
      expect(res.data.routes[0].route_id).toBe('r1');
      expect(res.data.routes[0].breakers[0].state).toBe('closed');
      expect(res.data.routes[0].breakers[1].state).toBe('open');
    }
  });

  it('stats 解析五个统计字段', async () => {
    const client = new ApiClient({
      fetchImpl: stubFetch([
        {
          json: {
            success: true,
            data: {
              version: 7,
              failed_reloads: 1,
              route_count: 4,
              breaker_count: 6,
              breaker_open_count: 2,
            },
            error: null,
          },
        },
      ]),
    });
    const res = await client.stats();
    expect(res.ok).toBe(true);
    if (res.ok) {
      expect(res.data).toEqual({
        version: 7,
        failed_reloads: 1,
        route_count: 4,
        breaker_count: 6,
        breaker_open_count: 2,
      });
    }
  });

  const resetCases: Array<{
    name: string;
    req: ResetBreakerRequest;
    expectedBody: Record<string, unknown>;
  }> = [
    {
      name: '路由级重置显式发送 instance_id: null',
      req: { route_id: 'r1' },
      expectedBody: { route_id: 'r1', instance_id: null },
    },
    {
      name: '实例级重置发送 instance_id 字符串',
      req: { route_id: 'r1', instance_id: 'i1' },
      expectedBody: { route_id: 'r1', instance_id: 'i1' },
    },
    {
      name: 'route_id 两侧空白被裁剪',
      req: { route_id: ' r1 ' },
      expectedBody: { route_id: 'r1', instance_id: null },
    },
  ];

  it.each(resetCases)(
    'resetBreakers $name',
    async ({ req, expectedBody }) => {
      const fetchImpl = stubFetch([
        { json: { success: true, data: { reset: 2 }, error: null } },
      ]);
      const client = new ApiClient({ fetchImpl });
      const res = await client.resetBreakers(req);
      expect(res.ok).toBe(true);
      expect(res.ok && res.data.reset).toBe(2);
      const mock = fetchImpl as unknown as ReturnType<typeof vi.fn>;
      expect(mock).toHaveBeenCalledTimes(1);
      const call = mock.mock.calls[0] as unknown as [string, RequestInit];
      expect(call[0]).toBe('/mgmt/v1/breakers/reset');
      expect(call[1].method).toBe('POST');
      expect(JSON.parse(String(call[1].body))).toEqual(expectedBody);
    }
  );

  it('resetBreakers route_id 为空时边界校验快速失败，不发送请求', async () => {
    const fetchImpl = stubFetch([
      { json: { success: true, data: { reset: 1 }, error: null } },
    ]);
    const client = new ApiClient({ fetchImpl });
    const res = await client.resetBreakers({ route_id: '   ' });
    expect(res.ok).toBe(false);
    expect(!res.ok && res.error).toBe('请求参数无效');
    expect(fetchImpl as unknown as ReturnType<typeof vi.fn>).not.toHaveBeenCalled();
  });

  it.each(['', '   '])(
    'resetBreakers 空白 instance_id(%j) 必须拒绝，不得放大为路由级全量重置',
    async (instanceId) => {
      const fetchImpl = stubFetch([
        { json: { success: true, data: { reset: 1 }, error: null } },
      ]);
      const client = new ApiClient({ fetchImpl });
      const res = await client.resetBreakers({ route_id: 'r1', instance_id: instanceId });
      expect(res.ok).toBe(false);
      expect(!res.ok && res.error).toBe('请求参数无效');
      expect(fetchImpl as unknown as ReturnType<typeof vi.fn>).not.toHaveBeenCalled();
    }
  );

  it('metrics 返回原始文本，不受信封约束', async () => {
    const client = new ApiClient({
      fetchImpl: stubFetch([{ text: 'rgateway_active_requests 5\n' }]),
    });
    const res = await client.metrics();
    expect(res.ok).toBe(true);
    expect(res.ok && res.data).toBe('rgateway_active_requests 5\n');
  });

  it('服务端失败信封走 error 通道', async () => {
    const client = new ApiClient({
      fetchImpl: stubFetch([
        { json: { success: false, data: null, error: 'invalid request' } },
      ]),
    });
    const res = await client.health();
    expect(res.ok).toBe(false);
    expect(!res.ok && res.error).toBe('invalid request');
  });

  it('HTTP 非 2xx 走 error 通道（泛化文案）', async () => {
    const client = new ApiClient({
      fetchImpl: stubFetch([{ status: 500, json: {} }]),
    });
    const res = await client.snapshot();
    expect(res.ok).toBe(false);
    expect(!res.ok && res.error).toBe('请求失败');
  });

  it('网络异常走 error 通道，不抛出', async () => {
    const client = new ApiClient({
      fetchImpl: vi.fn(async () => {
        throw new Error('ECONNREFUSED');
      }) as unknown as typeof fetch,
    });
    const res = await client.health();
    expect(res.ok).toBe(false);
    expect(!res.ok && res.error).toBe('无法连接管理面服务');
  });

  it('非法 JSON 响应走 error 通道，不崩', async () => {
    const fetchImpl = vi.fn(async () => ({
      ok: true,
      status: 200,
      json: async () => {
        throw new Error('bad json');
      },
      text: async () => 'not-json',
    })) as unknown as typeof fetch;
    const client = new ApiClient({ fetchImpl });
    const res = await client.stats();
    expect(res.ok).toBe(false);
    expect(!res.ok && res.error).toBe('请求失败');
  });

  it('非对象路由行被过滤，不污染结果', async () => {
    const client = new ApiClient({
      fetchImpl: stubFetch([
        {
          json: {
            success: true,
            data: {
              routes: [
                { id: 'a', order: 0, uri: '/x', protocol: 'http' },
                null,
                'garbage',
                { order: 2 },
              ],
            },
            error: null,
          },
        },
      ]),
    });
    const res = await client.routes();
    expect(res.ok).toBe(true);
    expect(res.ok && res.data.routes).toHaveLength(2);
    expect(res.ok && res.data.routes[1].id).toBe('');
  });
});
