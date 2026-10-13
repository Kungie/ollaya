"""Export Decima (the e5-small encoder and the late-interaction scorer) to one weightless ONNX graph.

    uv run python -m ollaya_convert.families.decima.export [--model decima-small] [--out out/decima-small]

Graph (every question of a request in one run; docs/families/decima.md):
    inputs   state_ids     int64   [q, ls]  each question's state row, right-padded with the pad id
             state_mask    int64   [q, ls]  1 on the row's tokens
             option_ids    int64   [o, lo]  every option row, questions in order, right-padded
             option_mask   int64   [o, lo]  1 on the row's tokens
             option_state  int64   [o]      the state row (question) each option belongs to
    outputs  scores        float32 [o]      DecimaModel.choice_scores: the raw score of each option, before the
                                            temperature (the scorer's score plus the bi-encoder skip term)
             ordinal_g     float32 [o]      ord_g(z_k), the ordinal head's latent projection of each pooled option
             ordinal_gap   float32 [o]      ord_gap(z_k), its threshold-gap projection

The encoder runs on the state rows and on the option rows separately (as upstream encodes them), then
`choice_scores` reads each option's state through `option_state`. The ordinal head's two projections are linear,
so the graph returns them per option and the runtime takes the mean over a question's levels
(mean_k(ord_g(z_k)) = ord_g(mean_k z_k)) and finishes the cumulative-link head, as upstream's runtime.py does in numpy.

Every initializer references the author's `pytorch/encoder/model.safetensors` or `pytorch/head.safetensors` at the
pinned revision by byte offset (weightless_sharded.make_weightless); nothing is re-hosted. The export is checked
against the PyTorch reference (`ref.forward`, upstream's own modules) on the shared edge cases before it is written.
"""
from __future__ import annotations

import argparse
import json
import os
import shutil
import tempfile

import numpy as np
import onnx
import torch

from ...weightless_sharded import make_weightless, safetensors_source
from ..llm_common import cases
from ..llm_common import onnx_export as ox
from . import ref
from .check import decision_for, shared_rule, tokenizer
from .layout import DecimaLayout, LayoutError, graph_inputs

LAYOUT = "decima-late-interaction-v1"
# The same graph; option rows always keep the option whole (decima.json's choice_fit, decision.json's option_fit).
LAYOUT_FIT = "decima-late-interaction-v2"
INPUT_NAMES = ["state_ids", "state_mask", "option_ids", "option_mask", "option_state"]
OUTPUT_NAMES = ["scores", "ordinal_g", "ordinal_gap"]
OPSET = 20
# Largest export-vs-reference difference accepted on any output (the runtime gate is 1e-3 on the scores).
EXPORT_TOL = 1e-4


class Graph(torch.nn.Module):
    """Decima.logits' network for many questions at once: DecimaModel.encode on both sides, then choice_scores."""

    def __init__(self, model):
        super().__init__()
        self.m = model

    def forward(self, state_ids, state_mask, option_ids, option_mask, option_state):
        hs = self.m.encode(state_ids, state_mask)
        hc = self.m.encode(option_ids, option_mask)
        s, z = self.m.choice_scores(hs, state_mask, hc, option_mask, option_state)
        return s, self.m.ord_g(z).squeeze(-1), self.m.ord_gap(z).squeeze(-1)


SAMPLE = {
    "state": {"ticket": "The customer was charged twice for order A-104 and wants the duplicate refunded today.",
              "plan": "pro", "amount": 49.0},
    "questions": {
        "team": {"type": "choice", "instructions": "Which team handles this?",
                 "criteria": {"billing": "Payments or invoices", "technical": "Bugs or outages", "sales": None,
                              "other": "Anything else"}},
        "refund": {"type": "noul", "instructions": "Is a refund requested?"},
        "upset": {"type": "score", "instructions": "How upset is the customer?",
                  "criteria": ["calm", "a little annoyed", "annoyed", "angry", "furious, threatening to leave"]},
    },
}


def export_graph(model, inputs, path, max_state_tokens):
    args = tuple(torch.from_numpy(inputs[k]) for k in INPUT_NAMES)
    q = torch.export.Dim("questions", min=1, max=256)
    o = torch.export.Dim("options", min=1, max=65536)
    ls = torch.export.Dim("state_len", min=2, max=max(512, max_state_tokens))
    lo = torch.export.Dim("option_len", min=2, max=512)
    dyn = {"state_ids": {0: q, 1: ls}, "state_mask": {0: q, 1: ls}, "option_ids": {0: o, 1: lo},
           "option_mask": {0: o, 1: lo}, "option_state": {0: o}}
    with torch.no_grad():
        program = torch.onnx.export(Graph(model).eval(), args, dynamo=True, opset_version=OPSET,
                                    input_names=INPUT_NAMES, output_names=OUTPUT_NAMES, dynamic_shapes=dyn,
                                    optimize=True)
    program.save(path, external_data=True)
    onnx.checker.check_model(path, full_check=True)
    got = [i.name for i in onnx.load(path, load_external_data=False).graph.input]
    if got != INPUT_NAMES:
        raise RuntimeError("graph inputs %s != %s" % (got, INPUT_NAMES))


def verify(d, lay, graph, pad, requests):
    """The graph (ONNX Runtime, CPU) against ref.forward (upstream's modules, fp32) per question, all questions of a
    request in one run. -> (questions, max diff per output)."""
    import onnxruntime as ort

    sess = ort.InferenceSession(graph, providers=["CPUExecutionProvider"])
    worst = {k: 0.0 for k in OUTPUT_NAMES}
    n = 0
    for state, questions in requests:
        try:
            rows = lay.encode(state, questions)["questions"]
        except LayoutError:
            continue
        out = dict(zip(OUTPUT_NAMES, sess.run(OUTPUT_NAMES, graph_inputs(rows, pad))))
        k = 0
        for item in ref.encode(d, state, questions):
            want = ref.forward(d, item)
            m = len(item["option_ids"])
            for name, key in (("scores", "scores"), ("ordinal_g", "ordinal_g"), ("ordinal_gap", "ordinal_gap")):
                worst[name] = max(worst[name], float(np.abs(out[name][k:k + m] - want[key]).max()))
            k += m
            n += 1
    return n, worst


def export(slug, out_dir):
    m = ref.MODELS[slug]
    d = ref.load("cpu", slug)
    snap = ref.snapshot(slug)
    ckpt = ref.checkpoint(slug, snap)
    tok_json = os.path.join(ckpt, "encoder", "tokenizer.json")
    layout = decision_for(d)
    lay = DecimaLayout(tokenizer(tok_json), layout)
    pad = layout["special_tokens"]["pad"]
    rows = lay.encode(SAMPLE["state"], SAMPLE["questions"])["questions"]
    inputs = graph_inputs(rows, pad)
    shape = {k: list(v.shape) for k, v in inputs.items()}
    print("sample shapes", shape)

    tmp = tempfile.mkdtemp(prefix="decima-export-", dir=os.environ.get("OLLAYA_SCRATCH"))
    export_graph(d.model, inputs, os.path.join(tmp, "model.onnx"), d.cfg.max_state_tokens)
    sources = [safetensors_source("model.safetensors", os.path.join(ckpt, "encoder", "model.safetensors"),
                                  repo=m["repo"], revision=m["revision"], filename="pytorch/encoder/model.safetensors"),
               safetensors_source("head.safetensors", os.path.join(ckpt, "head.safetensors"),
                                  repo=m["repo"], revision=m["revision"], filename="pytorch/head.safetensors")]
    # Exporter names are module paths under Graph.m: the encoder's are model.safetensors' keys, the scorer's are
    # head.safetensors' keys.
    report = make_weightless(tmp, out_dir, sources, [("m.encoder.", ""), ("m.", "")], link=True, sidecars=())
    shutil.rmtree(tmp, ignore_errors=True)
    if report["inline_bytes"] > 64 * 1024:
        raise SystemExit("too much stays inline: %s" % report["inline"])
    shutil.copy(tok_json, os.path.join(out_dir, "tokenizer.json"))

    requests = [(state, qs) for _, state, qs in cases.edge_cases() if not shared_rule(qs)]
    requests.append((SAMPLE["state"], SAMPLE["questions"]))
    n, worst = verify(d, lay, os.path.join(out_dir, "model.onnx"), pad, requests)
    print("export vs reference on %d questions: max |diff| %s" % (n, {k: "%.1e" % v for k, v in worst.items()}))
    if max(worst.values()) > EXPORT_TOL:
        raise SystemExit("the export differs from the reference by more than %.0e" % EXPORT_TOL)

    decision = {
        "engine": "onnx", "family": "decima", "layout": LAYOUT_FIT if layout.get("option_fit") else LAYOUT,
        "upstream": {"repo": m["repo"], "revision": m["revision"], "tag": m["tag"], "checkpoint": ref.CHECKPOINT,
                     "code": ref.CODE, "license": "Apache-2.0", "base": m["base"]},
        "contract": {
            "inputs": {"state_ids": {"dtype": "int64", "shape": ["questions", "state_len"],
                                     "note": "each question's state row, right-padded with special_tokens.pad"},
                       "state_mask": {"dtype": "int64", "shape": ["questions", "state_len"], "note": "1 on tokens"},
                       "option_ids": {"dtype": "int64", "shape": ["options", "option_len"],
                                      "note": "every option row, questions in order, right-padded"},
                       "option_mask": {"dtype": "int64", "shape": ["options", "option_len"], "note": "1 on tokens"},
                       "option_state": {"dtype": "int64", "shape": ["options"],
                                        "note": "the question (state row) of each option"}},
            "outputs": {"scores": {"dtype": "float32", "shape": ["options"],
                                   "note": "raw option scores; choice and noul: softmax(scores / temperature)"},
                        "ordinal_g": {"dtype": "float32", "shape": ["options"], "note": "ord_g(z) per option"},
                        "ordinal_gap": {"dtype": "float32", "shape": ["options"], "note": "ord_gap(z) per option"}}},
        **layout,
        "option_logits": {"choice": "labels in the caller's order", "score": "levels in order", "noul": "[yes, no]"},
        "opset": OPSET,
    }
    calibration = {
        "temperature": [layout["temperature"], 1.0, layout["temperature"]],
        "temperature_by_options": {},
        "source": "pytorch/decima.json `temperature`, fitted by the author. Score questions take it inside the "
                  "ordinal head (decision.json `temperature`), so their slot here is 1.",
    }
    files = {"model": slug, "layers": [
        {"role": "graph", "path": "model.onnx", "hosted_by": "ollaya",
         "bytes": os.path.getsize(os.path.join(out_dir, "model.onnx")),
         "sha256": ox.sha256_file(os.path.join(out_dir, "model.onnx"))},
        ox.file_entry("weights", m["repo"], m["revision"], "pytorch/encoder/model.safetensors",
                      os.path.join(ckpt, "encoder", "model.safetensors"), location="model.safetensors"),
        ox.file_entry("weights/head", m["repo"], m["revision"], "pytorch/head.safetensors",
                      os.path.join(ckpt, "head.safetensors"), location="head.safetensors"),
        ox.file_entry("tokenizer", m["repo"], m["revision"], "pytorch/encoder/tokenizer.json", tok_json),
        {"role": "decision", "path": "decision.json", "hosted_by": "ollaya"},
        {"role": "calibration", "path": "calibration.json", "hosted_by": "ollaya"},
        ox.file_entry("license", m["repo"], m["revision"], "LICENSE", os.path.join(snap, "LICENSE"))],
        "weightless": {k: v for k, v in report.items() if k != "unused"},
        "unused_checkpoint_tensors": report["unused"],
        "export_check": {"questions": n, "max_abs_diff": worst}}
    # decision.json is written with ASCII escapes: the noul template's em dash stays `\u2014` in the file.
    with open(os.path.join(out_dir, "decision.json"), "w", encoding="utf-8") as f:
        json.dump(decision, f, indent=2, ensure_ascii=True)
        f.write("\n")
    ox.write_json(os.path.join(out_dir, "calibration.json"), calibration)
    ox.write_json(os.path.join(out_dir, "files.json"), files)
    print(json.dumps(report["stats"]), "graph MB %.2f" % (files["layers"][0]["bytes"] / 2**20),
          "inline bytes", report["inline_bytes"], "unused", report["unused"])


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--model", default=ref.DEFAULT, choices=sorted(ref.MODELS))
    ap.add_argument("--out", default=None, help="default: out/<model>")
    a = ap.parse_args()
    out = a.out or os.path.join(ref.OUT, a.model)
    os.makedirs(out, exist_ok=True)
    export(a.model, out)


if __name__ == "__main__":
    main()
