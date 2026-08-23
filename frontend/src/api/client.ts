/** THE typed API client.
 *
 *  Every backend call goes through request(): one place for base
 *  paths, error surfacing, and runtime shape validation. A backend
 *  field rename fails HERE with the endpoint named -- loudly at the
 *  boundary -- instead of surfacing as undefined.toFixed() somewhere
 *  in a render three components away.
 */

export class ApiError extends Error {
  constructor(
    public endpoint: string,
    public status: number,
    message: string,
  ) {
    super(`${endpoint}: ${message}`);
  }
}

/** A runtime validator for T: returns a description of the first
 *  problem, or null when the value conforms. The phantom parameter
 *  ties each validator to the type it certifies at request() call
 *  sites. Kept hand-rolled (no schema dependency); each endpoint's
 *  validator lives beside its type. */
export type Validator<T> = ((v: unknown) => string | null) & {
  __certifies?: T;
};

export async function request<T>(
  endpoint: string,
  validate: Validator<T>,
  init?: RequestInit,
): Promise<T> {
  let resp: Response;
  try {
    resp = await fetch(`/api${endpoint}`, init);
  } catch (e) {
    throw new ApiError(endpoint, 0, `network error: ${String(e)}`);
  }
  if (!resp.ok) {
    const body = await resp.text().catch(() => "");
    throw new ApiError(endpoint, resp.status, body || resp.statusText);
  }
  const data: unknown = await resp.json().catch(() => {
    throw new ApiError(endpoint, resp.status, "response is not JSON");
  });
  const problem = validate(data);
  if (problem !== null) {
    throw new ApiError(endpoint, resp.status, `shape mismatch: ${problem}`);
  }
  return data as T;
}

export function post<T>(
  endpoint: string,
  body: unknown,
  validate: Validator<T>,
): Promise<T> {
  return request(endpoint, validate, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
}

/* ------------------------- validator helpers ------------------------- */

export const isObject = (v: unknown): v is Record<string, unknown> =>
  typeof v === "object" && v !== null && !Array.isArray(v);

export function field(
  obj: Record<string, unknown>,
  key: string,
  kind: "string" | "number" | "boolean",
): string | null {
  const v = obj[key];
  if (typeof v !== kind) return `${key}: expected ${kind}, got ${typeof v}`;
  return null;
}

export function arrayOf<T>(
  v: unknown,
  item: Validator<T>,
  label: string,
): string | null {
  if (!Array.isArray(v)) return `${label}: expected array`;
  for (let i = 0; i < v.length; i++) {
    const p = item(v[i]);
    if (p !== null) return `${label}[${i}].${p}`;
  }
  return null;
}
