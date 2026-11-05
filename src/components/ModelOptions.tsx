import { useStore } from "../lib/store";

/**
 * The `<option>`s of a model select: every model grouped by provider, the default marked, and
 * a "Select model" placeholder while nothing is chosen. The select around it sets the look.
 */
export function ModelOptions({ value }: { value: string | null }) {
  const models = useStore((s) => s.models);
  const providers = useStore((s) => s.providers);
  const defaultModelRef = useStore((s) => s.defaultModelRef);
  return (
    <>
      {value === null && <option value="">Select model</option>}
      {providers.map((p) => {
        const ms = models.filter((m) => m.providerId === p.id);
        if (ms.length === 0) return null;
        return (
          <optgroup key={p.id} label={p.name}>
            {ms.map((m) => (
              <option key={m.id} value={m.id}>
                {m.displayName}
                {m.id === defaultModelRef ? " (default)" : ""}
              </option>
            ))}
          </optgroup>
        );
      })}
    </>
  );
}
