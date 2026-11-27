import { describe, expect, it, vi } from "vitest";
import { act, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { json, renderWithQuery, stubFetch, target } from "./test/helpers";

/* The session gate, end to end in the DOM. Every test loads a fresh
 * module graph, so each gets its own session owner and choke point. */

/** The fake server: an answer, or undefined for a 404. */
type Handler = (method: string, path: string) => Response | undefined;

const live = {
  auth_enabled: true,
  user: "ops",
  idle_deadline_ms: Date.now() + 30 * 60_000,
  absolute_deadline_ms: Date.now() + 600 * 60_000,
  server_time_ms: Date.now(),
};

/** Answers for the data the shell polls with no targets configured. */
function data(path: string): Response | undefined {
  if (path === "/api/targets") return json({ targets: [] });
  if (path === "/api/metrics") return json({ targets: {} });
  return undefined;
}

async function renderApp(handler: Handler) {
  vi.resetModules();
  const server = stubFetch(({ method, path }) => handler(method, path));
  const { default: App } = await import("./App");
  const { DialogProvider } = await import("./components/dialogs");
  const { apiFetch } = await import("./api/apiFetch");
  renderWithQuery(
    <DialogProvider>
      <App />
    </DialogProvider>,
  );
  return { calls: server.calls, apiFetch };
}

describe("App session gate", () => {
  it("with auth on and no session shows only the login page and asks for no data", async () => {
    const { calls } = await renderApp(
      () => new Response("missing token or session", { status: 401 }),
    );
    expect(await screen.findByLabelText("Password")).toBeInTheDocument();
    expect(screen.queryByRole("link", { name: "Dashboard" })).toBeNull();
    await act(() => new Promise((r) => setTimeout(r, 200)));
    expect(calls()).toEqual(["GET /api/auth/session"]);
  });

  it("with auth off renders the shell after one session call and never a login", async () => {
    const { calls } = await renderApp((_method, path) =>
      path.startsWith("/api/auth/")
        ? json({ auth_enabled: false })
        : data(path),
    );
    expect(
      await screen.findByRole("link", { name: "Dashboard" }),
    ).toBeInTheDocument();
    await act(() => new Promise((r) => setTimeout(r, 200)));
    expect(screen.queryByLabelText("Password")).toBeNull();
    expect(screen.queryByRole("button", { name: "Sign out" })).toBeNull();
    expect(calls().filter((c) => c.includes("/api/auth/"))).toEqual([
      "GET /api/auth/session",
    ]);
  });

  it("says it is connecting while the server cannot be reached, then shows the shell once it answers", async () => {
    let down = true;
    const { calls } = await renderApp((_method, path) => {
      if (down) throw new TypeError("Failed to fetch");
      return path.startsWith("/api/auth/")
        ? json({ auth_enabled: false })
        : data(path);
    });
    expect(
      await screen.findByText("Connecting to the server"),
    ).toBeInTheDocument();
    expect(screen.queryByLabelText("Password")).toBeNull();
    expect(screen.queryByText("Dashboard", { selector: "a" })).toBeNull();
    down = false;
    expect(
      await screen.findByRole(
        "link",
        { name: "Dashboard" },
        { timeout: 3_000 },
      ),
    ).toBeInTheDocument();
    expect(screen.queryByText("Connecting to the server")).toBeNull();
    expect(calls().filter((c) => c.includes("/api/auth/"))).toEqual([
      "GET /api/auth/session",
      "GET /api/auth/session",
    ]);
  });

  it("shows who is signed in; when the session ends, keeps the page unreadable under the sign-in form, with what was typed, and shows it again after signing in", async () => {
    let ended = false;
    const { apiFetch } = await renderApp((method, path) => {
      if (path === "/api/auth/session" && method === "POST") {
        ended = false;
        return json(live);
      }
      if (ended) return new Response("session ended", { status: 401 });
      return path === "/api/auth/session" ? json(live) : data(path);
    });
    await screen.findByRole("button", { name: "Sign out" });
    expect(screen.getByText("ops")).toBeInTheDocument();
    const user = userEvent.setup();
    await user.click(screen.getByRole("button", { name: "+ Add Target" }));
    const typed = screen.getByPlaceholderText("Name");
    await user.type(typed, "unsent-name");

    ended = true;
    await act(async () => {
      await apiFetch("/api/targets");
    });
    const dialog = await screen.findByRole("dialog", { name: "Sign in again" });
    expect(dialog).toBeVisible();
    expect(
      within(dialog).getByText(/Your session ended\./),
    ).toBeInTheDocument();
    // Nothing of the page can be read or reached while signed out...
    expect(screen.queryByRole("link", { name: "Dashboard" })).toBeNull();
    expect(screen.getByText("Dashboard", { selector: "a" })).not.toBeVisible();
    expect(typed).not.toBeVisible();
    expect(typed.closest("[inert]")).not.toBeNull();
    // ...and nothing of it is lost.
    expect(typed).toHaveValue("unsent-name");

    await user.type(within(dialog).getByLabelText("Password"), "pw{Enter}");
    await waitFor(() =>
      expect(
        screen.queryByRole("dialog", { name: "Sign in again" }),
      ).toBeNull(),
    );
    expect(screen.getByRole("link", { name: "Dashboard" })).toBeVisible();
    expect(typed).toBeVisible();
    expect(typed.closest("[inert]")).toBeNull();
    expect(typed).toHaveValue("unsent-name");
  });

  it("prompts to stay signed in before the idle deadline and keeps the session alive on request", async () => {
    const now = Date.now();
    const soon = {
      ...live,
      idle_deadline_ms: now + 90_000,
      absolute_deadline_ms: now + 600 * 60_000,
      server_time_ms: now,
    };
    const { calls } = await renderApp((_method, path) => {
      if (path === "/api/auth/session/refresh") {
        return json({ ...soon, idle_deadline_ms: Date.now() + 30 * 60_000 });
      }
      return path === "/api/auth/session" ? json(soon) : data(path);
    });
    const stay = await screen.findByRole("button", { name: "Stay signed in" });
    expect(screen.getByRole("status")).toHaveTextContent(
      /this session ends at \d\d:\d\d:\d\d UTC \(in 1:[23]\d\)/,
    );
    await act(async () => stay.click());
    await waitFor(() =>
      expect(
        screen.queryByRole("button", { name: "Stay signed in" }),
      ).toBeNull(),
    );
    expect(
      calls().filter((c) => c === "POST /api/auth/session/refresh"),
    ).toHaveLength(1);
  });

  it("states when the session ends before the absolute deadline", async () => {
    const now = Date.now();
    const ending = {
      ...live,
      idle_deadline_ms: null,
      absolute_deadline_ms: now + 5 * 60_000,
      server_time_ms: now,
    };
    await renderApp((_method, path) =>
      path === "/api/auth/session" ? json(ending) : data(path),
    );
    expect(await screen.findByRole("status")).toHaveTextContent(
      /This session ends at \d\d:\d\d:\d\d UTC \(in (4:5\d|5:00)\), its time limit/,
    );
    expect(screen.queryByRole("button", { name: "Stay signed in" })).toBeNull();
  });
});

describe("App sidebar link state", () => {
  it("names each target's link state on its dot, whatever the carrier", async () => {
    await renderApp((_method, path) => {
      if (path.startsWith("/api/auth/")) return json({ auth_enabled: false });
      if (path === "/api/targets") {
        return json({
          targets: [
            // Heard, its socket still bound: connected wins.
            target({ id: "target-0", connected: true, listening: true }),
            // Bound and waiting: a tcp-listen peer that has not dialed
            // in, or a UDP target not yet heard.
            target({ id: "target-1", listening: true }),
            target({ id: "target-2" }),
          ],
        });
      }
      return data(path);
    });
    expect(
      await screen.findByTitle("Listening for the target"),
    ).toBeInTheDocument();
    expect(screen.getByTitle("Connected")).toBeInTheDocument();
    expect(screen.getByTitle("Disconnected")).toBeInTheDocument();
  });
});
