import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import "./index.css";
import App from "./App";
import { DialogProvider } from "./components/dialogs";

// One cache for all server state. Structural sharing keeps object
// identity stable when a refetch returns identical data, which is what
// the old hand-rolled targetsEqual() diffing existed to do.
const queryClient = new QueryClient({
  defaultOptions: {
    queries: {
      refetchOnWindowFocus: false,
      retry: 1,
    },
  },
});

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <QueryClientProvider client={queryClient}>
      <DialogProvider>
        <App />
      </DialogProvider>
    </QueryClientProvider>
  </StrictMode>,
);
