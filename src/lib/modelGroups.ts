import type { AcpModels, Model, Provider } from "./ipc.ts";

type AgentModel = AcpModels["availableModels"][number];

export interface ModelGroup {
  /** Provider name; null for models the catalog does not know. */
  provider: string | null;
  models: AgentModel[];
}

/**
 * The agent's models by provider, for the model chip. Patchbay's own agent and Grok take their
 * models from the catalog (`pb-<id>` model ids), so each model's provider is known. Null when
 * the agent brings its own models, or when fewer than two providers supply them: one provider
 * gains nothing from a first level.
 */
export function modelsByProvider(
  fromCatalog: boolean,
  available: readonly AgentModel[],
  models: readonly Pick<Model, "id" | "providerId">[],
  providers: readonly Pick<Provider, "id" | "name">[],
): ModelGroup[] | null {
  if (!fromCatalog) return null;
  const groups: ModelGroup[] = [];
  for (const m of available) {
    const ref = m.modelId.replace(/^pb-/, "");
    const providerId = models.find((c) => c.id === ref)?.providerId;
    const provider = providers.find((p) => p.id === providerId)?.name ?? null;
    let g = groups.find((x) => x.provider === provider);
    if (!g) groups.push((g = { provider, models: [] }));
    g.models.push(m);
  }
  return groups.filter((g) => g.provider !== null).length >= 2 ? groups : null;
}
