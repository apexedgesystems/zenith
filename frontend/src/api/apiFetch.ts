/** THE request choke point: the only caller of fetch in the app.
 *
 *  Every request -- the typed request() wrapper, the query hooks, the
 *  pages -- goes through apiFetch, so one place sees every answer. It
 *  sends each request exactly once and never retries: a refused
 *  request fails visibly at its call site and is never replayed. A 401
 *  is reported to the session owner (api/session.ts), which ends the
 *  UI's session state with the reason; the response still goes back
 *  to the caller unchanged, so each page shows the refusal its own
 *  way. The browser attaches the session cookie by itself
 *  (same-origin), so call sites hold no credentials.
 */

/** How the session owner hears about refusals. generation() names
 *  the session a request is sent under; refused() receives that name
 *  when the request comes back 401, so an answer to a request sent
 *  before a re-sign-in cannot end the newer session. */
export interface RefusalListener {
  generation: () => number;
  refused: (sentUnder: number) => void;
}

let listener: RefusalListener | null = null;

/** Install (or remove, with null) the refusal listener. One owner. */
export function setRefusalListener(next: RefusalListener | null): void {
  listener = next;
}

export async function apiFetch(
  path: string,
  init?: RequestInit,
): Promise<Response> {
  const sentUnder = listener?.generation() ?? 0;
  const resp = await fetch(path, init);
  if (resp.status === 401) listener?.refused(sentUnder);
  return resp;
}
