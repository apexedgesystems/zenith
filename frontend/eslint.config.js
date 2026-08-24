// Flat config for ESLint 9. The react-hooks rules are the point:
// several shipped bugs lived behind exhaustive-deps suppressions that
// nothing ever checked, because the lint script existed without any
// config. Suppressions are allowed only with a justifying comment.
import tseslint from "typescript-eslint";
import reactHooks from "eslint-plugin-react-hooks";

export default tseslint.config(
  { ignores: ["dist/", "node_modules/", "*.config.*"] },
  ...tseslint.configs.recommended,
  {
    files: ["src/**/*.{ts,tsx}"],
    plugins: { "react-hooks": reactHooks },
    rules: {
      ...reactHooks.configs.recommended.rules,
      // The load-bearing rule: several shipped bugs were stale-dep
      // bugs. New violations block; suppressions need a justifying
      // comment.
      "react-hooks/exhaustive-deps": "error",
      // These newer rules flag the codebase's deliberate ref-mirror
      // and server-sync patterns wholesale; they stay visible as
      // warnings until those patterns are reworked deliberately.
      "react-hooks/set-state-in-effect": "warn",
      "react-hooks/refs": "warn",
      // tsc (strict, noUnusedLocals) already enforces these; keep the
      // linter focused on what the compiler cannot see.
      "@typescript-eslint/no-unused-vars": "off",
      "@typescript-eslint/no-explicit-any": "error",
    },
  },
);
