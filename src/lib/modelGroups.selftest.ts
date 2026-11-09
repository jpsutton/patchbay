/**
 * The model chip's provider groups. Run: node --experimental-strip-types src/lib/modelGroups.selftest.ts
 */
import { modelsByProvider } from "./modelGroups.ts";
import { groupItems, groupNames, stripGroupSuffix, topItems, type GroupedOption } from "./chipMenu.ts";

function assert(cond: unknown, msg: string): asserts cond {
  if (!cond) throw new Error(msg);
}
const eq = (a: unknown, b: unknown, msg: string) =>
  assert(JSON.stringify(a) === JSON.stringify(b), `${msg}: got ${JSON.stringify(a)}, want ${JSON.stringify(b)}`);

const providers = [
  { id: "p1", name: "Anthropic" },
  { id: "p2", name: "Anthropic CVP" },
  { id: "p3", name: "Grok" },
];
const catalog = [
  { id: "a", providerId: "p1" },
  { id: "b", providerId: "p2" },
  { id: "c", providerId: "p1" },
  { id: "d", providerId: "p3" },
];
const avail = (...ids: string[]) => ids.map((id) => ({ modelId: `pb-${id}`, name: id.toUpperCase() }));

// Models group by provider, in the order each provider first appears.
{
  const g = modelsByProvider(true, avail("a", "b", "c", "d"), catalog, providers);
  assert(g !== null, "two or more providers group");
  eq(g.map((x) => x.provider), ["Anthropic", "Anthropic CVP", "Grok"], "provider order");
  eq(g[0]!.models.map((m) => m.modelId), ["pb-a", "pb-c"], "a provider's models stay together");
}

// One provider, or an agent with its own models: no first level.
eq(modelsByProvider(true, avail("a", "c"), catalog, providers), null, "one provider stays flat");
eq(modelsByProvider(false, avail("a", "b"), catalog, providers), null, "own-model agent stays flat");

// A model the catalog lost does not vanish: it is kept, ungrouped.
{
  const g = modelsByProvider(true, avail("a", "b", "zzz"), catalog, providers);
  assert(g !== null, "two providers group");
  eq(g.map((x) => x.provider), ["Anthropic", "Anthropic CVP", null], "unknown model kept");
}

// Two-level menu rows.
const opts: GroupedOption[] = [
  { value: "pb-a", label: "A (Anthropic)", group: "Anthropic" },
  { value: "pb-b", label: "B", group: "Anthropic CVP" },
  { value: "pb-c", label: "C", group: "Anthropic" },
  { value: "pb-z", label: "Z" },
];
eq(groupNames(opts), ["Anthropic", "Anthropic CVP"], "group names");
eq(
  topItems(opts).map((o) => [o.value, !!o.drill, o.count]),
  [["Anthropic", true, 2], ["Anthropic CVP", true, 1], ["pb-z", false, undefined]],
  "first level: groups, then the ungrouped option",
);
eq(groupItems(opts, "Anthropic").map((o) => [o.value, o.label]), [["pb-a", "A"], ["pb-c", "C"]], "second level strips the provider suffix");
eq(stripGroupSuffix("(Anthropic)", "Anthropic"), "(Anthropic)", "a label that is only the suffix stays");
eq(stripGroupSuffix("Opus (Anthropic CVP)", "Anthropic"), "Opus (Anthropic CVP)", "another provider's suffix stays");

console.log("modelGroups selftest ok");
