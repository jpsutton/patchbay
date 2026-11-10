// Dev-only page (`/options-sheet-harness.html`): a narrow Composer whose footer chips live in the
// options sheet, for scripts/e2e-options-sheet.mjs to drive with real key events. Not part of any build.
import { createRoot } from "react-dom/client";
import { Composer } from "../components/Composer";
import "../index.css";

function Harness() {
  return (
    <div className="fixed inset-0 flex flex-col justify-end">
      {/* Controls behind the sheet: Tab must never reach them while it is open. */}
      <button id="behind-a" className="m-2 border border-border p-2">behind a</button>
      <button id="behind-b" className="m-2 border border-border p-2">behind b</button>
      <div style={{ width: 320 }}>
        <Composer
          onSend={() => {}}
          footer={
            <>
              <button id="chip-1" className="p-3 text-left">Model</button>
              <button id="chip-2" className="p-3 text-left">Effort</button>
            </>
          }
        />
      </div>
    </div>
  );
}

createRoot(document.getElementById("root")!).render(<Harness />);
