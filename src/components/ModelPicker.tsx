import { useStore } from "../lib/store";
import { ModelRing } from "../lib/cable";
import { ChipSelect } from "./ChipSelect";

export function ModelPicker({
  value,
  onChange,
  disabled,
}: {
  value: string | null;
  onChange: (ref: string) => void;
  disabled?: boolean;
}) {
  const models = useStore((s) => s.models);
  const providers = useStore((s) => s.providers);
  const setSettingsOpen = useStore((s) => s.setSettingsOpen);
  if (models.length === 0) {
    return (
      <button className="px-1 text-xs text-accent-ink hover:underline" onClick={() => setSettingsOpen(true)}>
        Add a model in Settings
      </button>
    );
  }
  return (
    <ChipSelect
      lead={<ModelRing id={value} />}
      mono
      aria-label="Model"
      className="max-w-64"
      value={value ?? ""}
      disabled={disabled}
      onChange={(e) => onChange(e.target.value)}
    >
      {value === null && <option value="">Select model</option>}
      {providers.map((p) => {
        const ms = models.filter((m) => m.providerId === p.id);
        if (ms.length === 0) return null;
        return (
          <optgroup key={p.id} label={p.name}>
            {ms.map((m) => (
              <option key={m.id} value={m.id}>
                {m.displayName}
              </option>
            ))}
          </optgroup>
        );
      })}
    </ChipSelect>
  );
}
