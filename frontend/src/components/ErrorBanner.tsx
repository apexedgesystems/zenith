/** The one error banner. Five byte-identical inline copies of this
 *  markup existed before; every page renders failures the same way
 *  now, dismissible on click. */
export default function ErrorBanner({
  error,
  onDismiss,
}: {
  error: string | null;
  onDismiss?: () => void;
}) {
  if (!error) return null;
  return (
    <div
      role="alert"
      className="rounded-lg p-3 mb-4 text-sm cursor-pointer"
      style={{
        backgroundColor: "rgba(248,81,73,0.1)",
        color: "var(--color-crit)",
        border: "1px solid rgba(248,81,73,0.3)",
      }}
      onClick={onDismiss}
    >
      {error}
    </div>
  );
}
