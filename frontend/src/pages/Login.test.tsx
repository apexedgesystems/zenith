import { describe, expect, it, vi } from "vitest";
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import LoginPage from "./Login";

describe("LoginPage", () => {
  it("asks for a user name and a masked password, and waits for both", () => {
    render(
      <LoginPage
        reason="none"
        defaultUser=""
        overlay={false}
        onSignIn={vi.fn()}
      />,
    );
    expect(screen.getByLabelText("User name")).toHaveValue("");
    expect(screen.getByLabelText("Password")).toHaveAttribute(
      "type",
      "password",
    );
    expect(screen.getByRole("button", { name: "Sign in" })).toBeDisabled();
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("shows a refusal, clears the password and keeps the user name", async () => {
    const onSignIn = vi.fn(async () => "Wrong user name or password.");
    render(
      <LoginPage
        reason="none"
        defaultUser=""
        overlay={false}
        onSignIn={onSignIn}
      />,
    );
    const user = userEvent.setup();
    await user.type(screen.getByLabelText("User name"), " ops ");
    await user.type(screen.getByLabelText("Password"), "nope");
    await user.click(screen.getByRole("button", { name: "Sign in" }));
    expect(onSignIn).toHaveBeenCalledWith("ops", "nope");
    expect(await screen.findByRole("alert")).toHaveTextContent(
      "Wrong user name or password.",
    );
    expect(screen.getByLabelText("Password")).toHaveValue("");
    expect(screen.getByLabelText("User name")).toHaveValue(" ops ");
  });

  it("signs in once on Enter and shows no error when it succeeds", async () => {
    const onSignIn = vi.fn(async () => null);
    render(
      <LoginPage
        reason="signed-out"
        defaultUser="ops"
        overlay={false}
        onSignIn={onSignIn}
      />,
    );
    expect(screen.getByText("You signed out.")).toBeInTheDocument();
    const user = userEvent.setup();
    await user.type(screen.getByLabelText("Password"), "pw{Enter}");
    expect(onSignIn).toHaveBeenCalledTimes(1);
    expect(onSignIn).toHaveBeenCalledWith("ops", "pw");
    expect(screen.queryByRole("alert")).toBeNull();
  });

  it("over an ended session is a dialog that says why and that nothing was resent", () => {
    render(
      <LoginPage reason="idle" defaultUser="ops" overlay onSignIn={vi.fn()} />,
    );
    const dialog = screen.getByRole("dialog", { name: "Sign in again" });
    expect(dialog).toHaveTextContent(
      "Your session ended after a period without input.",
    );
    expect(dialog).toHaveTextContent("nothing was resent");
    expect(screen.getByLabelText("User name")).toHaveValue("ops");
  });
});
