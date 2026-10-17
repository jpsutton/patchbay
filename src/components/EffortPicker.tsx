import { ChipSelect } from "./ChipSelect";
import { ActivityIcon } from "./icons";

const EFFORTS = ["default", "none", "minimal", "low", "medium", "high", "xhigh", "max"] as const;

/** Reasoning effort for the next messages; "default" uses the model's setting. */
export function EffortPicker({
  value,
  modelDefault,
  onChange,
}: {
  value: string;
  modelDefault: string | null;
  onChange: (v: string) => void;
}) {
  return (
    <ChipSelect
      lead={<ActivityIcon size={14} />}
      className="max-w-44"
      value={value}
      onChange={(e) => onChange(e.target.value)}
      title={`Reasoning effort for the next messages${modelDefault ? ` (model default: ${modelDefault})` : ""}. Models without reasoning ignore it.`}
      aria-label="Reasoning effort"
    >
      {EFFORTS.map((e) => (
        <option key={e} value={e}>
          {e === "default" ? `Thinking: ${modelDefault ?? "default"}` : `Thinking: ${e}`}
        </option>
      ))}
    </ChipSelect>
  );
}
