import { useCallback, useState, useRef } from "react";
import { fileToBase64 } from "../api/upload";

/* ----------------------------- Types ----------------------------- */

interface UploadResult {
  id: number;
  timestamp: string;
  fileName: string;
  remotePath: string;
  size: number;
  status: string;
  error?: string;
}

/* ----------------------------- Page ----------------------------- */

const MAX_FILE_SIZE = 10 * 1024 * 1024; // 10MB limit

export default function FileTransferPage({
  selectedTarget,
  targets,
}: {
  selectedTarget: string;
  targets: { id: string; name: string; host?: string }[];
}) {
  const [remotePath, setRemotePath] = useState("bank_a/");
  const [selectedFile, setSelectedFile] = useState<File | null>(null);
  const [uploading, setUploading] = useState(false);
  const [history, setHistory] = useState<UploadResult[]>([]);
  const [dragOver, setDragOver] = useState(false);
  // Monotonic ids from a ref: ids derived from history.length reissue
  // after the cap trims the list, cross-wiring entries (in Commanding,
  // one entry's interpret-as choice applied to another).
  const idCounter = useRef(0);

  const targetName =
    targets.find((t) => t.id === selectedTarget)?.name || selectedTarget;

  const uploadFile = useCallback(
    async (file: File) => {
      setUploading(true);
      const entry: UploadResult = {
        id: ++idCounter.current,
        timestamp: new Date().toISOString().split("T")[1].split(".")[0],
        fileName: file.name,
        remotePath: remotePath + file.name,
        size: file.size,
        status: "uploading",
      };

      try {
        // Read file as base64
        const encoded = await fileToBase64(file);
        if ("error" in encoded) {
          throw new Error(encoded.error);
        }
        const base64 = encoded.base64;

        const r = await fetch(`/api/targets/${selectedTarget}/upload`, {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({
            remote_path: remotePath + file.name,
            content_base64: base64,
          }),
        });

        if (r.ok) {
          const data = await r.json();
          entry.status = data.status_name || "SUCCESS";
        } else {
          entry.status = "FAILED";
          entry.error = await r.text();
        }
      } catch (e: unknown) {
        entry.status = "ERROR";
        entry.error = e instanceof Error ? e.message : String(e);
      }

      setHistory((h) => [entry, ...h].slice(0, 50));
      setUploading(false);
      setSelectedFile(null);
    },
    [selectedTarget, remotePath],
  );

  const handleDrop = useCallback(
    (e: React.DragEvent) => {
      e.preventDefault();
      setDragOver(false);
      if (e.dataTransfer.files.length > 1) {
        setHistory((h) => [
          {
            id: ++idCounter.current,
            timestamp: new Date().toISOString().split("T")[1].split(".")[0],
            fileName: "(multiple)",
            remotePath: "",
            size: 0,
            status: "REJECTED",
            error: "Only one file at a time. Drop a single file.",
          },
          ...h,
        ]);
        return;
      }
      const file = e.dataTransfer.files[0];
      if (file) {
        if (file.size > MAX_FILE_SIZE) {
          setHistory((h) => [
            {
              id: ++idCounter.current,
              timestamp: new Date().toISOString().split("T")[1].split(".")[0],
              fileName: file.name,
              remotePath: "",
              size: file.size,
              status: "REJECTED",
              error: `File too large (${(file.size / 1024 / 1024).toFixed(
                1,
              )}MB). Max 10MB.`,
            },
            ...h,
          ]);
          return;
        }
        setSelectedFile(file);
        uploadFile(file);
      }
    },
    [uploadFile],
  );

  const handleFileSelect = (e: React.ChangeEvent<HTMLInputElement>) => {
    const file = e.target.files?.[0];
    if (file) setSelectedFile(file);
  };

  return (
    <div className="max-w-4xl">
      <h1 className="text-xl font-bold mb-4">File Transfer</h1>

      {/* Upload area */}
      <div
        className="rounded-lg p-4 mb-5"
        style={{
          backgroundColor: "var(--color-surface)",
          border: "1px solid var(--color-border)",
        }}
      >
        <div
          className="text-xs uppercase tracking-wider mb-3"
          style={{ color: "var(--color-text-muted)" }}
        >
          Upload to {targetName}
        </div>

        {/* Destination path */}
        <div className="mb-3">
          <label
            className="text-[10px] uppercase tracking-wider mb-1 block"
            style={{ color: "var(--color-text-muted)" }}
          >
            Destination Path (relative to .apex_fs/)
          </label>
          <input
            value={remotePath}
            onChange={(e) => setRemotePath(e.target.value)}
            placeholder="bank_a/"
            className="mono text-sm w-full"
            style={{ padding: "6px 8px", maxWidth: "400px" }}
          />
        </div>

        {/* Drop zone */}
        <div
          onDragOver={(e) => {
            e.preventDefault();
            setDragOver(true);
          }}
          onDragLeave={() => setDragOver(false)}
          onDrop={handleDrop}
          className="rounded-lg p-8 text-center mb-3"
          style={{
            border: `2px dashed ${
              dragOver ? "var(--color-accent)" : "var(--color-border)"
            }`,
            backgroundColor: dragOver ? "rgba(88,166,255,0.05)" : "transparent",
            transition: "all 0.15s",
          }}
        >
          <div
            className="text-sm mb-2"
            style={{ color: "var(--color-text-secondary)" }}
          >
            Drag and drop a file here
          </div>
          <div
            className="text-xs mb-3"
            style={{ color: "var(--color-text-muted)" }}
          >
            or
          </div>
          <label
            className="text-sm px-4 py-1.5 rounded-md cursor-pointer"
            style={{
              backgroundColor: "var(--color-elevated)",
              color: "var(--color-text-primary)",
              border: "1px solid var(--color-border)",
            }}
          >
            Browse Files
            <input
              type="file"
              onChange={handleFileSelect}
              style={{ display: "none" }}
            />
          </label>
        </div>

        {/* Selected file + upload button */}
        {selectedFile && !uploading && (
          <div className="flex items-center gap-3">
            <span
              className="mono text-sm"
              style={{ color: "var(--color-text-secondary)" }}
            >
              {selectedFile.name} ({(selectedFile.size / 1024).toFixed(1)} KB)
            </span>
            <button
              onClick={() => uploadFile(selectedFile)}
              className="text-sm px-4 py-1.5 font-bold text-white rounded-md"
              style={{
                backgroundColor: "var(--color-accent)",
                border: "none",
                cursor: "pointer",
              }}
            >
              Upload
            </button>
          </div>
        )}

        {uploading && (
          <div className="text-sm" style={{ color: "var(--color-accent)" }}>
            Uploading...
          </div>
        )}
      </div>

      {/* Common destinations */}
      <div className="mb-5">
        <div
          className="text-xs uppercase tracking-wider mb-2"
          style={{ color: "var(--color-text-muted)" }}
        >
          Quick Destinations
        </div>
        <div className="flex flex-wrap gap-1.5">
          {[
            { label: "TPRM", path: "bank_a/tprm/" },
            { label: "Libraries", path: "bank_a/libs/" },
            { label: "RTS Scripts", path: "bank_a/rts/" },
            { label: "ATS Scripts", path: "bank_a/ats/" },
            { label: "Root", path: "" },
          ].map((dest) => (
            <button
              key={dest.label}
              onClick={() => setRemotePath(dest.path)}
              className="text-xs px-3 py-1.5 rounded-md"
              style={{
                backgroundColor:
                  remotePath === dest.path
                    ? "var(--color-accent)"
                    : "var(--color-elevated)",
                color:
                  remotePath === dest.path
                    ? "#fff"
                    : "var(--color-text-secondary)",
                border: "1px solid var(--color-border)",
                cursor: "pointer",
              }}
            >
              {dest.label}
            </button>
          ))}
        </div>
      </div>

      {/* Download / Export */}
      <div
        className="rounded-lg p-4 mb-5"
        style={{
          backgroundColor: "var(--color-surface)",
          border: "1px solid var(--color-border)",
        }}
      >
        <div
          className="text-xs uppercase tracking-wider mb-3"
          style={{ color: "var(--color-text-muted)" }}
        >
          Download
        </div>
        <div className="flex gap-2 mb-3">
          <button
            onClick={() =>
              window.open(
                `/api/targets/${selectedTarget}/telemetry/csv?limit=100000`,
                "_blank",
              )
            }
            className="text-sm px-4 py-1.5 font-bold text-white rounded-md"
            style={{
              backgroundColor: "var(--color-accent)",
              border: "none",
              cursor: "pointer",
            }}
          >
            Export Telemetry (CSV)
          </button>
        </div>
        <div className="text-xs" style={{ color: "var(--color-text-muted)" }}>
          Target filesystem downloads (logs, registry dumps) require direct
          access. Use SCP from a terminal:
        </div>
        <pre
          className="mono text-xs mt-2 p-2 rounded"
          style={{
            backgroundColor: "var(--color-elevated)",
            color: "var(--color-text-secondary)",
          }}
        >
          {`scp user@${
            targets.find((t) => t.id === selectedTarget)?.host || "target-host"
          }:~/.apex_fs/logs/ ./logs/`}
        </pre>
      </div>

      {/* Transfer history */}
      <div
        className="text-xs uppercase tracking-wider mb-2"
        style={{ color: "var(--color-text-muted)" }}
      >
        Transfer History ({history.length})
      </div>
      {history.length === 0 ? (
        <div className="text-sm" style={{ color: "var(--color-text-muted)" }}>
          No transfers yet.
        </div>
      ) : (
        <div className="flex flex-col gap-1.5">
          {history.map((entry) => {
            const isOk = entry.status === "SUCCESS";
            return (
              <div
                key={entry.id}
                className="rounded-md px-3 py-2 mono text-xs"
                style={{
                  border: `1px solid ${
                    isOk ? "var(--color-ok)" : "var(--color-crit)"
                  }`,
                  backgroundColor: isOk
                    ? "rgba(63,185,80,0.05)"
                    : "rgba(248,81,73,0.08)",
                }}
              >
                <div className="flex justify-between">
                  <span>
                    <span style={{ color: "var(--color-text-muted)" }}>
                      {entry.timestamp}
                    </span>{" "}
                    <span className="font-bold">{entry.fileName}</span>{" "}
                    <span style={{ color: "var(--color-text-muted)" }}>
                      -&gt; {entry.remotePath}
                    </span>{" "}
                    <span style={{ color: "var(--color-text-muted)" }}>
                      ({(entry.size / 1024).toFixed(1)} KB)
                    </span>
                  </span>
                  <span
                    className="font-bold"
                    style={{
                      color: isOk ? "var(--color-ok)" : "var(--color-crit)",
                    }}
                  >
                    {entry.status}
                  </span>
                </div>
                {entry.error && (
                  <div className="mt-1" style={{ color: "var(--color-crit)" }}>
                    {entry.error}
                  </div>
                )}
              </div>
            );
          })}
        </div>
      )}
    </div>
  );
}
