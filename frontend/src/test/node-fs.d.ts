/* Tests run under Node, and the command-encoder tests read the shipped
 * target catalogs from the repository with readFileSync. The browser
 * build never imports node:fs; this declares the one function those
 * tests use. */
declare module "node:fs" {
  export function readFileSync(path: URL, encoding: "utf8"): string;
}
