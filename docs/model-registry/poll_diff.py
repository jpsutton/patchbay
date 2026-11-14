#!/usr/bin/env python3
"""poll_diff.py — poll the public model feeds and diff them against the
last snapshot, for the Patchbay model registry (reasoning tiers).

Feeds (all public; no scraping of provider pages):
  1. models.dev
       GET https://models.dev/api.json                       (no key)
       Identity, pricing, context, reasoning + effort options,
       release/last-updated dates, status (alpha/beta/deprecated).
  2. OpenRouter
       GET https://openrouter.ai/api/v1/models               (no key)
       Live availability, pricing, context, modalities, and an
       expiration_date on models being retired.
  3. Artificial Analysis (optional — needs a free API key)
       GET https://artificialanalysis.ai/api/v2/language/models/free
       header: x-api-key: $ARTIFICIAL_ANALYSIS_API_KEY
       Free tier: headline indices (Intelligence Index v5), median
       performance, input/output pricing. The full /models endpoint
       is Pro+; this script only uses the free endpoint. If the key
       is unset the AA feed is skipped, not failed.

What it checks, per pinned model in watchlist.json:
  - price changes (input / output / cache) on models.dev and OpenRouter
  - status flips (especially -> deprecated) on models.dev
  - reasoning-effort options changing on models.dev
  - context-window changes
  - Artificial Analysis Intelligence Index movement (key set only)
  - OpenRouter expiration_date appearing (deprecation signal)
Plus, for watched provider families: any model whose release_date is
newer than the last run (new releases in the same families).

Reliability notes (learned 2026-11-10): the OpenRouter list endpoint
is not stable run to run — one fetch returned 469 models with the three
Claude 5.x models absent, the next fetch had them. So the script
re-fetches a feed once when it matches fewer pinned models than
expected, and a model that is not seen in a feed this run keeps its
previous snapshot values with a note — it is never reported as removed
or repriced on one missing fetch.

State lives in state/snapshot.json next to this script. The first run
writes the baseline and reports nothing as "changed"; later runs diff
against it. Run monthly, or on any review trigger from the registry doc
(patchbay-coordinator-model-recommendation.md).

Usage:
  python3 poll_diff.py                 fetch, diff vs snapshot, save new snapshot
  python3 poll_diff.py --check         fetch all feeds, validate shape, no diff, no write
  python3 poll_diff.py --no-write      fetch + diff, do not save the snapshot
  python3 poll_diff.py --state-dir DIR --watchlist FILE

Exit codes: 0 = ok (at least one feed worked); 1 = every feed failed.
Python 3.9+ stdlib only — no dependencies, runs on the Mac or this VM.
"""

import argparse
import json
import os
import sys
import urllib.error
import urllib.request
from datetime import datetime, timezone

MODELS_DEV_URL = "https://models.dev/api.json"
OPENROUTER_URL = "https://openrouter.ai/api/v1/models"
AA_URL = "https://artificialanalysis.ai/api/v2/language/models/free"
USER_AGENT = "patchbay-model-registry/1.0 (monthly registry scan)"
TIMEOUT = 60


def fetch_json(url, headers=None, attempts=2):
    """Return (data, error). error is None on success. Retries once."""
    last_err = None
    for _ in range(attempts):
        req = urllib.request.Request(url, headers={"User-Agent": USER_AGENT,
                                                  **(headers or {})})
        try:
            with urllib.request.urlopen(req, timeout=TIMEOUT) as resp:
                return json.loads(resp.read().decode("utf-8")), None
        except urllib.error.HTTPError as e:
            last_err = "HTTP %s" % e.code
        except urllib.error.URLError as e:
            last_err = "network: %s" % e.reason
        except (ValueError, UnicodeDecodeError) as e:
            last_err = "bad JSON: %s" % e
    return None, last_err


# ---- models.dev -------------------------------------------------------

def models_dev_entries(data):
    """Yield (provider_id, model_id, entry) for every model in models.dev."""
    for provider_id, provider in data.items():
        if not isinstance(provider, dict):
            continue
        for model_id, entry in provider.get("models", {}).items():
            if isinstance(entry, dict):
                yield provider_id, model_id, entry


def find_models_dev(entries, pinned):
    """Preferred-provider entry only: a reseller's price is a different
    data point, so never silently fall back to another provider."""
    want = pinned["models_dev_id"]
    pref = pinned.get("models_dev_provider")
    cands = [(p, m, e) for (p, m, e) in entries
             if m == want
             or str(e.get("canonical_model_id", "")).endswith("/" + want)]
    if not cands:
        return None, "not found in models.dev feed"
    if pref:
        for cand in cands:
            if cand[0] == pref:
                return cand, None
        return None, ("preferred provider '%s' entry missing this run" % pref)
    return cands[0], None


def extract_models_dev(provider_id, entry):
    cost = entry.get("cost") or {}
    limit = entry.get("limit") or {}
    efforts = None
    has_toggle = False
    for opt in entry.get("reasoning_options") or []:
        if not isinstance(opt, dict):
            continue
        if opt.get("type") == "effort":
            efforts = opt.get("values")
        elif opt.get("type") == "toggle":
            has_toggle = True
    if efforts is None and has_toggle:
        efforts = ["toggle"]
    return {
        "provider": provider_id,
        "price_in": cost.get("input"),
        "price_out": cost.get("output"),
        "cache_read": cost.get("cache_read"),
        "cache_write": cost.get("cache_write"),
        "context": limit.get("context"),
        "output_limit": limit.get("output"),
        "reasoning": entry.get("reasoning"),
        "efforts": efforts,
        "status": entry.get("status"),
        "release_date": entry.get("release_date"),
        "last_updated": entry.get("last_updated"),
    }


# ---- OpenRouter ---------------------------------------------------------

def find_openrouter(models, pinned):
    want = pinned["models_dev_id"]
    for m in models:
        mid = m.get("id", "")
        if mid == want or mid.endswith("/" + want):
            return m, None
    return None, "not found in OpenRouter feed"


def _per_mtok(value):
    try:
        return round(float(value) * 1_000_000, 4)
    except (TypeError, ValueError):
        return None


def extract_openrouter(m):
    pricing = m.get("pricing") or {}
    return {
        "id": m.get("id"),
        "price_in": _per_mtok(pricing.get("prompt")),
        "price_out": _per_mtok(pricing.get("completion")),
        "context": m.get("context_length"),
        "created": m.get("created"),
        "expiration_date": m.get("expiration_date"),
    }


# ---- Artificial Analysis -------------------------------------------------

def find_aa(models, pinned):
    slug = pinned.get("aa_slug")
    if slug:
        for m in models:
            if m.get("slug") == slug:
                return m, None
    name = (pinned.get("aa_name") or "").lower()
    if name:
        hits = [m for m in models
                if name in str(m.get("name", "")).lower()]
        if len(hits) == 1:
            return hits[0], None
        if len(hits) > 1:
            return None, ("ambiguous AA name match (%d hits); "
                          "set aa_slug in watchlist.json" % len(hits))
    return None, "not found in AA feed"


def extract_aa(m):
    index = None
    for key, value in m.items():
        if "intelligence_index" in key and isinstance(value, (int, float)):
            index = value
            break
    return {
        "slug": m.get("slug"),
        "name": m.get("name"),
        "intelligence_index": index,
    }


# ---- diff ----------------------------------------------------------------

def _fmt(value):
    if isinstance(value, list):
        return ",".join(str(v) for v in value)
    return value


def diff_model(prev, cur):
    """Field-level changes between two extracted dicts. Returns [str]."""
    out = []
    for key in sorted(set(prev) | set(cur)):
        old, new = prev.get(key), cur.get(key)
        if old == new:
            continue
        if key in ("price_in", "price_out", "cache_read", "cache_write",
                   "intelligence_index"):
            try:
                if old is not None and new is not None \
                        and abs(float(old) - float(new)) < 0.00005:
                    continue
            except (TypeError, ValueError):
                pass
        out.append("  %s: %s -> %s" % (key, _fmt(old), _fmt(new)))
    return out


def new_models(prev_seen, entries, watch_providers, since_date):
    """Models in watched providers released since the last snapshot."""
    out = []
    if prev_seen is None:
        return out  # first run: baseline, nothing is "new"
    for provider_id, model_id, entry in entries:
        if provider_id not in watch_providers:
            continue
        rel = entry.get("release_date")
        if not rel or (since_date and rel < since_date):
            continue
        if model_id in prev_seen.get(provider_id, []):
            continue
        out.append((provider_id, model_id, rel, entry.get("name")))
    return sorted(out, key=lambda t: (t[2], t[0], t[1]))


def count_matches(kind, items, pinned_list):
    """How many pinned models a fetched feed actually contains."""
    n = 0
    for pinned in pinned_list:
        if kind == "models_dev":
            found, _ = find_models_dev(items, pinned)
        else:
            found, _ = find_openrouter(items, pinned)
        if found:
            n += 1
    return n


# ---- main ------------------------------------------------------------------

def load_snapshot(state_dir):
    path = os.path.join(state_dir, "snapshot.json")
    if not os.path.exists(path):
        return None
    with open(path, encoding="utf-8") as f:
        return json.load(f)


def save_snapshot(state_dir, snapshot):
    os.makedirs(state_dir, exist_ok=True)
    path = os.path.join(state_dir, "snapshot.json")
    if os.path.exists(path):
        with open(path, encoding="utf-8") as f:
            prev = f.read()
        with open(os.path.join(state_dir, "snapshot.prev.json"),
                  "w", encoding="utf-8") as f:
            f.write(prev)
    with open(path, "w", encoding="utf-8") as f:
        json.dump(snapshot, f, indent=2, sort_keys=True)


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--watchlist", default=None)
    ap.add_argument("--state-dir", default=None)
    ap.add_argument("--check", action="store_true",
                    help="fetch feeds and validate shape; no diff, no write")
    ap.add_argument("--no-write", action="store_true",
                    help="fetch + diff, do not save the snapshot")
    args = ap.parse_args(argv)

    here = os.path.dirname(os.path.abspath(__file__))
    watchlist_path = args.watchlist or os.path.join(here, "watchlist.json")
    state_dir = args.state_dir or os.path.join(here, "state")
    with open(watchlist_path, encoding="utf-8") as f:
        watchlist = json.load(f)
    pinned_list = watchlist["models"]

    prev = None if args.check else load_snapshot(state_dir)
    prev_models = prev.get("models", {}) if prev else {}

    now = datetime.now(timezone.utc)
    stamp = now.strftime("%Y-%m-%dT%H:%MZ")
    print("Model registry poll — %s" % stamp)

    feeds = {}   # name -> (ok, detail)
    current = {}  # label -> {"models_dev": {...}|None, ...}
    notes = []

    # --- models.dev (re-fetch once if it matches fewer pinned models) ---
    md_data, md_err = fetch_json(MODELS_DEV_URL)
    md_entries = []
    if md_data is not None:
        md_entries = list(models_dev_entries(md_data))
        if count_matches("models_dev", md_entries, pinned_list) < len(pinned_list):
            retry_data, _ = fetch_json(MODELS_DEV_URL)
            if retry_data is not None:
                retry_entries = list(models_dev_entries(retry_data))
                if count_matches("models_dev", retry_entries,
                                 pinned_list) > count_matches(
                                     "models_dev", md_entries, pinned_list):
                    md_entries = retry_entries
        feeds["models.dev"] = (True, "%d models" % len(md_entries))
    else:
        feeds["models.dev"] = (False, md_err)

    # --- OpenRouter (same re-fetch rule) ---
    or_data, or_err = fetch_json(OPENROUTER_URL)
    or_models = []
    if or_data is not None and isinstance(or_data.get("data"), list):
        or_models = or_data["data"]
        if count_matches("openrouter", or_models, pinned_list) < len(pinned_list):
            retry_data, _ = fetch_json(OPENROUTER_URL)
            if retry_data is not None and isinstance(
                    retry_data.get("data"), list):
                if count_matches("openrouter", retry_data["data"],
                                 pinned_list) > count_matches(
                                     "openrouter", or_models, pinned_list):
                    or_models = retry_data["data"]
        feeds["OpenRouter"] = (True, "%d models" % len(or_models))
    else:
        feeds["OpenRouter"] = (False, or_err or "unexpected response shape")

    # --- Artificial Analysis (optional) ---
    aa_key = os.environ.get("ARTIFICIAL_ANALYSIS_API_KEY")
    aa_models = []
    if aa_key:
        aa_data, aa_err = fetch_json(
            AA_URL, headers={"x-api-key": aa_key})
        if aa_data is not None and isinstance(aa_data.get("data"), list):
            aa_models = aa_data["data"]
            feeds["Artificial Analysis"] = (
                True, "%d models (free endpoint)" % len(aa_models))
        else:
            feeds["Artificial Analysis"] = (
                False, aa_err or "unexpected response shape")
    else:
        feeds["Artificial Analysis"] = (
            None, "skipped — set ARTIFICIAL_ANALYSIS_API_KEY to enable")

    for name, (ok, detail) in feeds.items():
        mark = "OK" if ok is True else ("SKIP" if ok is None else "FAIL")
        print("  feed %-22s %s — %s" % (name, mark, detail))

    # --- per-model extraction ---
    # A model not seen in a feed this run keeps its previous snapshot
    # values with a note; it is never diffed as removed or repriced.
    for pinned in pinned_list:
        label = pinned["label"]
        row = {}
        old_row = prev_models.get(label, {})
        if md_entries:
            found, note = find_models_dev(md_entries, pinned)
            if found:
                row["models_dev"] = extract_models_dev(found[0], found[2])
            elif old_row.get("models_dev"):
                row["models_dev"] = old_row["models_dev"]
                notes.append("%s: not seen in models.dev this run — "
                             "kept previous values" % label)
            else:
                row["models_dev"] = None
                notes.append("%s: %s" % (label, note))
        if or_models:
            found, note = find_openrouter(or_models, pinned)
            if found:
                row["openrouter"] = extract_openrouter(found)
            elif old_row.get("openrouter"):
                row["openrouter"] = old_row["openrouter"]
                notes.append("%s: not seen in OpenRouter this run — "
                             "kept previous values" % label)
            else:
                row["openrouter"] = None
                notes.append("%s: %s" % (label, note))
        if aa_models:
            found, note = find_aa(aa_models, pinned)
            if found:
                row["aa"] = extract_aa(found)
            elif old_row.get("aa"):
                row["aa"] = old_row["aa"]
                notes.append("%s: not seen in AA this run — "
                             "kept previous values" % label)
            else:
                row["aa"] = None
                notes.append("%s: %s" % (label, note))
        current[label] = row

    print()
    print("Pinned models:")
    for pinned in pinned_list:
        label = pinned["label"]
        row = current[label]
        md = row.get("models_dev") or {}
        orr = row.get("openrouter") or {}
        aa = row.get("aa") or {}
        print("  %-18s %s" % (label, pinned.get("role", "")))
        print("    models.dev: $%s/$%s in/out, status=%s, efforts=%s" % (
            md.get("price_in"), md.get("price_out"),
            md.get("status") or "current", _fmt(md.get("efforts"))))
        print("    OpenRouter: %s $%s/$%s in/out, expires=%s" % (
            orr.get("id") or "—", orr.get("price_in"), orr.get("price_out"),
            orr.get("expiration_date")))
        if aa:
            print("    AA Index: %s" % aa.get("intelligence_index"))
        elif aa_key:
            print("    AA Index: not matched")

    if notes:
        print()
        print("Notes:")
        for note in notes:
            print("  - %s" % note)

    if args.check:
        print()
        print("Check done — no diff, no snapshot written.")
        return 0 if any(ok for ok, _ in feeds.values()) else 1

    changes = []
    if prev is None:
        print()
        print("Baseline snapshot written — first run, nothing to diff "
              "against. Run again after the next feed change to see diffs.")
    else:
        for pinned in pinned_list:
            label = pinned["label"]
            old = prev_models.get(label, {})
            new = current[label]
            for feed in ("models_dev", "openrouter", "aa"):
                o, n = old.get(feed) or {}, new.get(feed) or {}
                if o == n:
                    continue
                field_changes = diff_model(o, n)
                if field_changes:
                    changes.append("%s (%s):" % (label, feed))
                    if not o:
                        changes.append("  (first seen in this feed)")
                    changes.extend(field_changes)
        print()
        if changes:
            print("Changes since %s:" % prev.get("generated_at", "?"))
            print("\n".join(changes))
        else:
            print("No changes since %s." % prev.get("generated_at", "?"))

    # new releases in watched families
    seen = prev.get("seen", {}) if prev else None
    since = prev.get("generated_at", "")[:10] if prev else None
    fresh = new_models(seen, md_entries, watchlist.get("watch_providers", []),
                       since)
    print()
    if fresh:
        print("New models in watched families since %s:" % (since or "—"))
        for provider_id, model_id, rel, name in fresh:
            print("  %s/%s — %s (released %s)" % (provider_id, model_id,
                                                  name or "", rel))
    elif prev is not None:
        print("No new models in watched families since %s." % since)

    if not args.no_write:
        snapshot = {
            "generated_at": stamp,
            "models": current,
            "seen": {
                "models_dev": {},
            },
        }
        for provider_id, model_id, entry in md_entries:
            snapshot["seen"]["models_dev"].setdefault(provider_id, []).append(
                model_id)
        for provider_id in snapshot["seen"]["models_dev"]:
            snapshot["seen"]["models_dev"][provider_id].sort()
        save_snapshot(state_dir, snapshot)
        print()
        print("Snapshot saved: %s" % os.path.join(state_dir, "snapshot.json"))

    ok_feeds = [ok for ok, _ in feeds.values() if ok is True]
    return 0 if ok_feeds else 1


if __name__ == "__main__":
    sys.exit(main())
