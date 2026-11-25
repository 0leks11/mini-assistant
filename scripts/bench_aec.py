#!/usr/bin/env python3
"""Frozen M-AEC + Spectrogram + Hill-Climb measurement harness for GPT Mini.

Runs `aec_bench` on fixed hardware ALSA captures (`hw:0,2` 8ch S32_LE @ 48 kHz),
computes Hann-windowed 512-pt STFT matrices and 6-band ERLE breakdowns, renders
Waterfall SVG spectrograms to `.hillclimb/spectrograms/`, and logs full run telemetry
to `.hillclimb/runs/` and `.hillclimb/decision.tsv`.
"""

from __future__ import annotations

import argparse
import cmath
import datetime
import json
import math
import os
import shutil
import statistics
import subprocess
import sys
import wave
import struct
from pathlib import Path

try:
    import numpy as np  # type: ignore
except ImportError:  # pragma: no cover
    np = None


BANDS = [
    ("Sub-bass", 60.0, 180.0),
    ("Low-mid", 180.0, 500.0),
    ("Mid", 500.0, 1500.0),
    ("Presence", 1500.0, 4000.0),
    ("Brilliance", 4000.0, 8000.0),
    ("Full", 60.0, 8000.0),
]

FFT_N = 512
HOP_N = 256
FS = 16000


def read_wav_f64(path: Path) -> list[float]:
    with wave.open(str(path), "rb") as wf:
        assert wf.getnchannels() == 1
        assert wf.getsampwidth() == 2
        assert wf.getframerate() == FS
        frames = wf.readframes(wf.getnframes())
    samples = struct.unpack(f"<{len(frames) // 2}h", frames)
    return [s / 32768.0 for s in samples]


def _fft_radix2(buf: list[complex]) -> list[complex]:
    n = len(buf)
    if n <= 1:
        return buf
    even = _fft_radix2(buf[0::2])
    odd = _fft_radix2(buf[1::2])
    out = [0j] * n
    factor = -2j * math.pi / n
    for k in range(n // 2):
        t = cmath.exp(factor * k) * odd[k]
        out[k] = even[k] + t
        out[k + n // 2] = even[k] - t
    return out


def compute_stft_db(samples: list[float]) -> tuple[list[list[float]], list[float]]:
    """Returns (frames_db [T][FFT_N//2 + 1], hann_coherent_norm)."""
    n_frames = max(0, (len(samples) - FFT_N) // HOP_N + 1)
    if np is not None:
        arr = np.asarray(samples, dtype=np.float64)
        win = np.hanning(FFT_N)
        win_norm = float(np.sum(win) * 0.5)
        frames_db: list[list[float]] = []
        for idx in range(n_frames):
            seg = arr[idx * HOP_N : idx * HOP_N + FFT_N] * win
            spec = np.abs(np.fft.rfft(seg)) / win_norm
            db = 20.0 * np.log10(np.maximum(spec, 1e-6))
            frames_db.append([round(float(x), 2) for x in db])
        freqs = np.fft.rfftfreq(FFT_N, 1.0 / FS).tolist()
        return frames_db, freqs

    win = [0.5 * (1.0 - math.cos(2.0 * math.pi * i / (FFT_N - 1))) for i in range(FFT_N)]
    win_norm = sum(win) * 0.5
    frames_db = []
    for idx in range(n_frames):
        seg = [complex(samples[idx * HOP_N + i] * win[i], 0.0) for i in range(FFT_N)]
        spec = _fft_radix2(seg)[: FFT_N // 2 + 1]
        row = []
        for c in spec:
            mag = abs(c) / win_norm
            row.append(round(20.0 * math.log10(max(mag, 1e-6)), 2))
        frames_db.append(row)
    freqs = [k * FS / FFT_N for k in range(FFT_N // 2 + 1)]
    return frames_db, freqs


def band_energies_db(
    frames_db: list[list[float]],
    freqs: list[float],
    eval_start_ms: int,
    eval_end_ms: int,
) -> dict[str, float]:
    out: dict[str, float] = {}
    start_idx = max(0, eval_start_ms // 16)
    end_idx = min(len(frames_db), (eval_end_ms + 15) // 16)
    if end_idx <= start_idx:
        start_idx, end_idx = 0, len(frames_db)
    window_frames = frames_db[start_idx:end_idx]

    for name, lo, hi in BANDS:
        bin_indices = [k for k, f in enumerate(freqs) if lo <= f < hi or (hi == 8000.0 and f == 8000.0)]
        if not window_frames or not bin_indices:
            out[name] = -120.0
            continue
        pwr_sum = 0.0
        for frame in window_frames:
            for b in bin_indices:
                pwr_sum += 10.0 ** (frame[b] / 10.0)
        mean_pwr = pwr_sum / len(window_frames)
        out[name] = round(10.0 * math.log10(max(mean_pwr, 1e-12)), 2)
    return out


def viridis_like(val_db: float, vmin: float = -85.0, vmax: float = -10.0) -> str:
    t = max(0.0, min(1.0, (val_db - vmin) / (vmax - vmin)))
    stops = [
        (0.0, (12, 16, 36)),
        (0.25, (38, 42, 118)),
        (0.50, (28, 128, 142)),
        (0.75, (118, 202, 82)),
        (1.0, (253, 231, 37)),
    ]
    for idx in range(len(stops) - 1):
        t0, c0 = stops[idx]
        t1, c1 = stops[idx + 1]
        if t <= t1:
            alpha = (t - t0) / max(t1 - t0, 1e-9)
            r = int(c0[0] + alpha * (c1[0] - c0[0]))
            g = int(c0[1] + alpha * (c1[1] - c0[1]))
            b = int(c0[2] + alpha * (c1[2] - c0[2]))
            return f"#{r:02x}{g:02x}{b:02x}"
    return "#fde725"


def render_waterfall_svg(
    out_svg: Path,
    run_id: str,
    summary: dict,
    before_db: list[list[float]],
    ref_db: list[list[float]],
    after_db: list[list[float]],
    freqs: list[float],
    band_delta: dict[str, float],
    metrics_rows: list[dict],
) -> None:
    width = 1160
    pad_l = 85
    plot_w = 820
    panel_h = 155
    gap = 42
    top_y = 75

    # Map frequencies 60..8000 Hz into 64 vertical rows
    n_rows = 64
    f_lo, f_hi = 60.0, 8000.0
    row_bins: list[int] = []
    for r in range(n_rows):
        # y from top (high freq) to bottom (low freq)
        frac = 1.0 - (r + 0.5) / n_rows
        target_f = f_lo + frac * (f_hi - f_lo)
        best_b = min(range(len(freqs)), key=lambda b: abs(freqs[b] - target_f))
        row_bins.append(best_b)

    t_frames = min(len(before_db), len(ref_db), len(after_db))
    cell_w = plot_w / max(t_frames, 1)
    cell_h = panel_h / n_rows

    panels = [
        ("1. Mic Before AEC (16 kHz STFT, dBFS)", before_db),
        ("2. Hardware Loopback Ref (Ch6 TDMIN_B, dBFS)", ref_db),
        ("3. Mic After M-AEC + RES (Output 16 kHz, dBFS)", after_db),
    ]

    erle_chart_y = top_y + 3 * (panel_h + gap) + 15
    erle_chart_h = 120
    total_h = erle_chart_y + erle_chart_h + 70

    svg: list[str] = [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{total_h}" viewBox="0 0 {width} {total_h}">',
        '<rect width="100%" height="100%" fill="#0d1117"/>',
        f'<text x="24" y="32" fill="#e6edf3" font-family="monospace" font-size="18" font-weight="bold">GPT Mini M-AEC Hillclimb Run {run_id}: ERLE {summary["erle_db"]:.2f} dB (Steady {summary["steady_erle_db"]:.2f} dB) | CPU {summary["aec_cpu_1core_pct"]:.2f}% (1-core)</text>',
        f'<text x="24" y="54" fill="#8b949e" font-family="monospace" font-size="12">ch={summary["channel_mode"]} | delay={summary["aec_config"]["bulk_delay_samples"]} samp | mu={summary["aec_config"]["step_size"]} | leak={summary["aec_config"]["power_leak"]} | eps={summary["aec_config"]["regularization"]} | res={summary["aec_config"]["res_enabled"]} (floor={summary["aec_config"]["res_floor_db"]}dB, od={summary["aec_config"]["res_overdrive"]}) | DT={summary["double_talk_blocks"]}/{summary["eval_blocks"]}</text>',
    ]

    eval_x0 = pad_l + (summary["eval_start_ms"] / max(summary["audio_duration_ms"], 1)) * plot_w
    eval_x1 = pad_l + (summary["eval_end_ms"] / max(summary["audio_duration_ms"], 1)) * plot_w

    for p_idx, (title, mat) in enumerate(panels):
        py = top_y + p_idx * (panel_h + gap)
        svg.append(f'<text x="{pad_l}" y="{py - 10}" fill="#c9d1d9" font-family="monospace" font-size="13" font-weight="bold">{title}</text>')
        for ti in range(t_frames):
            x = pad_l + ti * cell_w
            frame = mat[ti]
            for ri, b in enumerate(row_bins):
                y = py + ri * cell_h
                color = viridis_like(frame[b])
                svg.append(f'<rect x="{x:.1f}" y="{y:.1f}" width="{cell_w + 0.6:.1f}" height="{cell_h + 0.6:.1f}" fill="{color}"/>')
        svg.append(f'<rect x="{pad_l}" y="{py}" width="{plot_w}" height="{panel_h}" fill="none" stroke="#30363d" stroke-width="1"/>')
        svg.append(f'<line x1="{eval_x0:.1f}" y1="{py}" x2="{eval_x0:.1f}" y2="{py + panel_h}" stroke="#ff7b72" stroke-dasharray="4,3" stroke-width="1.2"/>')
        svg.append(f'<line x1="{eval_x1:.1f}" y1="{py}" x2="{eval_x1:.1f}" y2="{py + panel_h}" stroke="#ff7b72" stroke-dasharray="4,3" stroke-width="1.2"/>')
        for freq_lbl, y_frac in [("8k", 0.0), ("4k", 0.5), ("2k", 0.75), ("60Hz", 1.0)]:
            ly = py + y_frac * panel_h
            svg.append(f'<text x="{pad_l - 8}" y="{ly + 4:.1f}" text-anchor="end" fill="#8b949e" font-family="monospace" font-size="11">{freq_lbl}</text>')

    # Right sidebar: 6-band ERLE summary
    sb_x = pad_l + plot_w + 24
    svg.append(f'<rect x="{sb_x}" y="{top_y}" width="215" height="{3 * panel_h + 2 * gap}" fill="#161b22" stroke="#30363d" rx="6"/>')
    svg.append(f'<text x="{sb_x + 14}" y="{top_y + 24}" fill="#e6edf3" font-family="monospace" font-size="13" font-weight="bold">6-Band ERLE (dB)</text>')
    for b_idx, (b_name, b_lo, b_hi) in enumerate(BANDS):
        by = top_y + 58 + b_idx * 78
        val = band_delta.get(b_name, 0.0)
        bar_w = max(0.0, min(160.0, (val / 35.0) * 160.0))
        bar_col = "#3fb950" if val >= 20.0 else ("#d29922" if val >= 10.0 else "#f85149")
        svg.append(f'<text x="{sb_x + 14}" y="{by}" fill="#c9d1d9" font-family="monospace" font-size="12">{b_name} ({int(b_lo)}-{int(b_hi)}Hz)</text>')
        svg.append(f'<text x="{sb_x + 195}" y="{by}" text-anchor="end" fill="{bar_col}" font-family="monospace" font-size="12" font-weight="bold">{val:+.1f} dB</text>')
        svg.append(f'<rect x="{sb_x + 14}" y="{by + 8}" width="180" height="12" fill="#21262d" rx="2"/>')
        svg.append(f'<rect x="{sb_x + 14}" y="{by + 8}" width="{bar_w:.1f}" height="12" fill="{bar_col}" rx="2"/>')

    # Bottom ERLE(t) + Double-Talk chart
    svg.append(f'<text x="{pad_l}" y="{erle_chart_y - 10}" fill="#c9d1d9" font-family="monospace" font-size="13" font-weight="bold">4. Block ERLE(t) [dB] and Double-Talk Gate (-5..+40 dB)</text>')
    svg.append(f'<rect x="{pad_l}" y="{erle_chart_y}" width="{plot_w}" height="{erle_chart_h}" fill="#161b22" stroke="#30363d"/>')
    zero_y = erle_chart_y + (40.0 / 45.0) * erle_chart_h
    target_y = erle_chart_y + (15.0 / 45.0) * erle_chart_h  # 25 dB
    svg.append(f'<line x1="{pad_l}" y1="{zero_y:.1f}" x2="{pad_l + plot_w}" y2="{zero_y:.1f}" stroke="#484f58" stroke-dasharray="3,3"/>')
    svg.append(f'<line x1="{pad_l}" y1="{target_y:.1f}" x2="{pad_l + plot_w}" y2="{target_y:.1f}" stroke="#3fb950" stroke-dasharray="5,4"/>')
    svg.append(f'<text x="{pad_l - 8}" y="{target_y + 4:.1f}" text-anchor="end" fill="#3fb950" font-family="monospace" font-size="10">25dB</text>')
    svg.append(f'<text x="{pad_l - 8}" y="{zero_y + 4:.1f}" text-anchor="end" fill="#8b949e" font-family="monospace" font-size="10">0dB</text>')

    pts: list[str] = []
    n_m = max(len(metrics_rows), 1)
    for idx, row in enumerate(metrics_rows):
        x = pad_l + (idx / n_m) * plot_w
        erle = max(-5.0, min(40.0, float(row["inst_erle_db"])))
        y = erle_chart_y + ((40.0 - erle) / 45.0) * erle_chart_h
        pts.append(f"{x:.1f},{y:.1f}")
        if row.get("double_talk"):
            svg.append(f'<rect x="{x:.1f}" y="{erle_chart_y}" width="{max(2.0, plot_w / n_m):.1f}" height="8" fill="#ff7b72" opacity="0.8"/>')
    if pts:
        svg.append(f'<polyline fill="none" stroke="#58a6ff" stroke-width="1.8" points="{" ".join(pts)}"/>')

    # Time axis labels
    dur_ms = summary["audio_duration_ms"]
    for frac in [0.0, 0.25, 0.5, 0.75, 1.0]:
        tx = pad_l + frac * plot_w
        lbl = f"{int(frac * dur_ms)}ms"
        svg.append(f'<text x="{tx:.1f}" y="{erle_chart_y + erle_chart_h + 18}" text-anchor="middle" fill="#8b949e" font-family="monospace" font-size="11">{lbl}</text>')

    svg.append("</svg>")
    out_svg.write_text("\n".join(svg), encoding="utf-8")


def main() -> int:
    parser = argparse.ArgumentParser(description="GPT Mini Frozen M-AEC & Spectrogram Harness")
    parser.add_argument("--run-id", required=True)
    parser.add_argument("--input", default="../scratch/probe_A_TDMIN_B.raw")
    parser.add_argument("--channel", default="ch0")
    parser.add_argument("--eval-start-ms", type=int, default=800)
    parser.add_argument("--eval-end-ms", type=int, default=2150)
    parser.add_argument("--steady-offset-ms", type=int, default=400)
    parser.add_argument("--aec-enabled", default="true")
    parser.add_argument("--bulk-delay", type=int, default=None)
    parser.add_argument("--step-size", type=float, default=None)
    parser.add_argument("--power-leak", type=float, default=None)
    parser.add_argument("--regularization", type=float, default=None)
    parser.add_argument("--max-attenuation-db", type=float, default=None)
    parser.add_argument("--res-enabled", default=None)
    parser.add_argument("--res-floor-db", type=float, default=None)
    parser.add_argument("--res-overdrive", type=float, default=None)
    parser.add_argument("--mic-gain", type=float, default=1.0)
    parser.add_argument("--kws-model", default=None)
    parser.add_argument("--repeats", type=int, default=3)
    parser.add_argument("--hypothesis", default="")
    parser.add_argument("--files-changed", default="")
    parser.add_argument("--baseline", default="-")
    parser.add_argument("--status", default=None, help="KEPT, REVERTED, or baseline")
    parser.add_argument("--commit-or-patch", default="-")
    parser.add_argument("--notes", default="")
    parser.add_argument("--run-checks", action="store_true")
    args = parser.parse_args()

    script_dir = Path(__file__).resolve().parent
    crate_dir = script_dir.parent
    repo_root = crate_dir.parent
    hill_dir = repo_root / ".hillclimb"
    run_dir = hill_dir / "runs" / f"run_{args.run_id}"
    spec_dir = hill_dir / "spectrograms"
    run_dir.mkdir(parents=True, exist_ok=True)
    spec_dir.mkdir(parents=True, exist_ok=True)

    # Ensure release binary built cleanly once per code revision
    build_proc = subprocess.run(
        ["cargo", "build", "--release", "--example", "aec_bench"],
        cwd=crate_dir,
        capture_output=True,
        text=True,
        check=False,
    )
    if build_proc.returncode != 0:
        sys.stderr.write(build_proc.stdout + build_proc.stderr)
        return build_proc.returncode

    bench_bin = crate_dir / "target" / "release" / "examples" / "aec_bench"
    input_path = (crate_dir / args.input).resolve() if not Path(args.input).is_absolute() else Path(args.input)

    cmd = [
        str(bench_bin),
        "--input", str(input_path),
        "--out-dir", str(run_dir),
        "--channel", args.channel,
        "--eval-start-ms", str(args.eval_start_ms),
        "--eval-end-ms", str(args.eval_end_ms),
        "--steady-offset-ms", str(args.steady_offset_ms),
        "--aec-enabled", str(args.aec_enabled),
        "--mic-gain", str(args.mic_gain),
    ]
    if args.bulk_delay is not None:
        cmd += ["--bulk-delay", str(args.bulk_delay)]
    if args.step_size is not None:
        cmd += ["--step-size", str(args.step_size)]
    if args.power_leak is not None:
        cmd += ["--power-leak", str(args.power_leak)]
    if args.regularization is not None:
        cmd += ["--regularization", str(args.regularization)]
    if args.max_attenuation_db is not None:
        cmd += ["--max-attenuation-db", str(args.max_attenuation_db)]
    if args.res_enabled is not None:
        cmd += ["--res-enabled", str(args.res_enabled)]
    if args.res_floor_db is not None:
        cmd += ["--res-floor-db", str(args.res_floor_db)]
    if args.res_overdrive is not None:
        cmd += ["--res-overdrive", str(args.res_overdrive)]
    if args.kws_model is not None:
        cmd += ["--kws-model", str(args.kws_model)]

    cpu_samples = []
    summaries = []
    for _ in range(max(1, args.repeats)):
        res = subprocess.run(cmd, cwd=crate_dir, capture_output=True, text=True, check=False)
        if res.returncode != 0:
            sys.stderr.write(res.stdout + res.stderr)
            return res.returncode
        s = json.loads(res.stdout)
        summaries.append(s)
        cpu_samples.append(s["aec_cpu_1core_pct"])

    summary = summaries[-1]
    summary["median_aec_cpu_1core_pct"] = round(statistics.median(cpu_samples), 3)

    # STFT spectrogram analysis
    before_pcm = read_wav_f64(run_dir / "mic_before_16k.wav")
    ref_pcm = read_wav_f64(run_dir / "ref_loopback_16k.wav")
    after_pcm = read_wav_f64(run_dir / "aec_after_16k.wav")

    before_stft, freqs = compute_stft_db(before_pcm)
    ref_stft, _ = compute_stft_db(ref_pcm)
    after_stft, _ = compute_stft_db(after_pcm)

    b_bands = band_energies_db(before_stft, freqs, args.eval_start_ms, args.eval_end_ms)
    a_bands = band_energies_db(after_stft, freqs, args.eval_start_ms, args.eval_end_ms)
    r_bands = band_energies_db(ref_stft, freqs, args.eval_start_ms, args.eval_end_ms)
    b_steady = band_energies_db(before_stft, freqs, args.eval_start_ms + args.steady_offset_ms, args.eval_end_ms)
    a_steady = band_energies_db(after_stft, freqs, args.eval_start_ms + args.steady_offset_ms, args.eval_end_ms)

    band_delta = {k: round(b_bands[k] - a_bands[k], 2) for k in b_bands}
    steady_band_delta = {k: round(b_steady[k] - a_steady[k], 2) for k in b_steady}

    metrics_rows = [
        json.loads(line)
        for line in (run_dir / "metrics.jsonl").read_text(encoding="utf-8").splitlines()
        if line.strip()
    ]

    matrix_path = spec_dir / f"run_{args.run_id}_stft_matrix.json"
    matrix_payload = {
        "run_id": args.run_id,
        "summary": summary,
        "bands_eval_window": {
            "before_dbfs": b_bands,
            "ref_dbfs": r_bands,
            "after_dbfs": a_bands,
            "erle_db": band_delta,
        },
        "bands_steady_window": {
            "before_dbfs": b_steady,
            "after_dbfs": a_steady,
            "erle_db": steady_band_delta,
        },
        "freqs_hz": [round(f, 2) for f in freqs],
        "before_db": before_stft,
        "ref_db": ref_stft,
        "after_db": after_stft,
    }
    matrix_path.write_text(json.dumps(matrix_payload), encoding="utf-8")

    svg_path = spec_dir / f"run_{args.run_id}_waterfall.svg"
    render_waterfall_svg(svg_path, args.run_id, summary, before_stft, ref_stft, after_stft, freqs, band_delta, metrics_rows)

    tests_status = "SKIPPED"
    if args.run_checks:
        chk = subprocess.run([str(script_dir / "check.sh")], cwd=crate_dir, capture_output=True, text=True, check=False)
        tests_status = "PASS" if chk.returncode == 0 else "FAIL"
        (run_dir / "check.log").write_text(chk.stdout + chk.stderr, encoding="utf-8")

    raw_out = hill_dir / "runs" / f"run_{args.run_id}_raw.json"
    raw_out.write_text(json.dumps({"summary": summary, "bands": matrix_payload["bands_eval_window"], "steady_bands": matrix_payload["bands_steady_window"]}, indent=2), encoding="utf-8")

    log_lines = [
        f"=== Hillclimb Run {args.run_id} ({datetime.datetime.now(datetime.timezone.utc).isoformat()}) ===",
        f"Hypothesis: {args.hypothesis}",
        f"Input: {input_path} | Channel: {summary['channel_mode']} | Window: {args.eval_start_ms}..{args.eval_end_ms} ms",
        f"ERLE: {summary['erle_db']:.2f} dB | Steady ERLE: {summary['steady_erle_db']:.2f} dB | Double-Talk: {summary['double_talk_blocks']}/{summary['eval_blocks']}",
        f"Host CPU median (1-core): {summary['median_aec_cpu_1core_pct']:.2f}%",
        f"Band ERLE (eval): {band_delta}",
        f"Band ERLE (steady): {steady_band_delta}",
        f"Spectrogram SVG: {svg_path}",
        f"Spectrogram JSON: {matrix_path}",
        f"Checks: {tests_status}",
    ]
    log_text = "\n".join(log_lines) + "\n"
    (hill_dir / "runs" / f"run_{args.run_id}.log").write_text(log_text, encoding="utf-8")
    (hill_dir / "runs" / "last_run.log").write_text(log_text, encoding="utf-8")
    sys.stdout.write(log_text)

    if args.status is not None:
        tsv = hill_dir / "decision.tsv"
        ts = datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="seconds")
        try:
            base_f = float(args.baseline)
            res_f = summary["erle_db"]
            delta_s = f"{((res_f - base_f) / max(abs(base_f), 1e-6)) * 100.0:+.1f}%"
        except ValueError:
            delta_s = f"+{summary['erle_db']:.2f}dB"
        row = "\t".join([
            args.run_id,
            ts,
            args.hypothesis,
            args.files_changed,
            str(args.baseline),
            f"{summary['erle_db']:.2f} (steady {summary['steady_erle_db']:.2f})",
            delta_s,
            tests_status,
            args.status,
            args.commit_or_patch,
            args.notes or f"steady={summary['steady_erle_db']:.2f}dB, bands={band_delta}",
        ])
        with tsv.open("a", encoding="utf-8") as fp:
            fp.write(row + "\n")

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
