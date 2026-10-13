"""Golden fixtures for the Rust port of `decima-late-interaction-v1` and `-v2`, from the author's `decima/model.py` in fp32.

    uv run python -m ollaya_convert.families.decima.goldens OUT_DIR [--td-limit 40] [--device cpu]

OUT_DIR is the export (its decision.json, which names the model, and tokenizer.json). Writes goldens-<model>.jsonl next
to it, one JSON line per request of the shared set (the edge cases and --td-limit typed-decisions rows, as the shared request file
has them), `check.extra_cases` and `check.fit_cases`:
    {"id", "state", "questions",
     "error": null | {"code": "INVALID_REQUEST" | "STATE_TRUNCATED", "message"},   upstream /v1/systemone's 422
     "rows": [{"qid", "type", "state_ids", "option_ids", "state_truncated"}],
     "plan": [{"qid", "type", "keys", "scores", "ordinal_g", "ordinal_gap", "log_probs", "probabilities"}],
     "answers": {qid: answer}}                                                       upstream system_one's answers
Rows and plans are in upstream's option order (a choice's labels, score levels, noul [yes, no]); `scores` are the raw
scores before the temperature, `log_probs` upstream's calibrated log-probabilities, `probabilities` their softmax.
A request upstream answers with STATE_TRUNCATED keeps its rows and plan: model.py keeps the state row's first
tokens, which is what Ollaya's /api/decide answers with. A request rejected for an invalid question has no rows and
is followed by "<id>#valid" with the questions upstream accepts on their own. Requests Ollaya's shared question
parser rejects are skipped. Every row is checked against the layout port before it is written.
"""
from __future__ import annotations

import argparse
import json
import os
import time

import numpy as np

from . import ref
from .check import all_cases, compare, shared_rule, tokenizer
from .layout import DecimaLayout


def softmax(z):
    z = np.asarray(z, dtype=np.float64)
    e = np.exp(z - z.max())
    return (e / e.sum()).tolist()


def floats(a):
    return [float(x) for x in a]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("model_dir")
    ap.add_argument("--td-limit", type=int, default=40)
    ap.add_argument("--device", default="cpu")
    ap.add_argument("--out", default=None)
    a = ap.parse_args()
    decision = json.load(open(os.path.join(a.model_dir, "decision.json"), encoding="utf-8"))
    slug = next(k for k, m in ref.MODELS.items() if m["repo"] == decision["upstream"]["repo"])
    d = ref.load(a.device, slug)
    lay = DecimaLayout(tokenizer(os.path.join(a.model_dir, "tokenizer.json")), decision)
    path = a.out or os.path.join(os.path.dirname(os.path.abspath(a.model_dir)), "goldens-%s.jsonl" % slug)

    def record(cid, state, questions):
        verdict = compare(d, lay, state, questions)
        code, message = ref.verdict(d, state, questions)
        rec = {"id": cid, "state": state, "questions": questions,
               "error": {"code": code, "message": message} if code else None}
        if verdict == "rejected":
            return rec, False
        assert verdict == "identical", (cid, verdict)
        items = ref.encode(d, state, questions)
        rec["rows"] = [{k: it[k] for k in ("qid", "type", "state_ids", "option_ids", "state_truncated")}
                       for it in items]
        rec["plan"] = []
        for it in items:
            out = ref.forward(d, it)
            rec["plan"].append({"qid": it["qid"], "type": it["type"], "keys": it["keys"],
                                "scores": floats(out["scores"]), "ordinal_g": floats(out["ordinal_g"]),
                                "ordinal_gap": floats(out["ordinal_gap"]), "log_probs": floats(out["log_probs"]),
                                "probabilities": softmax(out["log_probs"])})
        rec["answers"] = ref.answers(d, state, questions)
        return rec, True

    n, t0 = 0, time.time()
    with open(path, "w", encoding="utf-8") as f:
        for cid, state, questions in all_cases(a.td_limit):
            if shared_rule(questions):
                continue
            rec, accepted = record(cid, state, questions)
            f.write(json.dumps(rec, ensure_ascii=False) + "\n")
            n += 1
            if not accepted and isinstance(questions, dict):
                valid = {}
                for qid, q in questions.items():
                    try:
                        ref.encode(d, state, {qid: q})
                        valid[qid] = q
                    except ref.RequestError:
                        pass
                if valid:
                    rec, accepted = record(cid + "#valid", state, valid)
                    if accepted:
                        f.write(json.dumps(rec, ensure_ascii=False) + "\n")
                        n += 1
            print("%4d %-50s %5.1fs" % (n, cid, time.time() - t0), flush=True)
    print("wrote %d records to %s" % (n, path))


if __name__ == "__main__":
    main()
