import { useContext } from "react";
import { ChipRows, ChipSelect } from "./ChipSelect";
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
  // The options sheet labels the row, so its values drop the "Thinking:" prefix.
  const prefix = useContext(ChipRows) ? "" : "Thinking: ";
  return (
    <ChipSelect
      lead={<ActivityIcon size={14} />}
      className="max-w-44"
      value={value}
      onChange={(e) => onChange(e.target.value)}
      title={`Reasoning effort for the next messages${modelDefault ? ` (model default: ${modelDefault})` : ""}. Models without reasoning ignore it.`}
      aria-label="Reasoning effort"
      label="Thinking"
    >
      {EFFORTS.map((e) => (
        <option key={e} value={e}>
          {`${prefix}${e === "default" ? (modelDefault ?? "default") : e}`}
        </option>
      ))}
    </ChipSelect>
  );
}
