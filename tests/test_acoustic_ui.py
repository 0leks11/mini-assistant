"""Offline browser-logic fixtures. No microphone, HTTP listener or external service."""

import json
from pathlib import Path
import re
import shutil
import subprocess
import unittest


ROOT = Path(__file__).resolve().parents[1]
NODE = shutil.which("node")
SOURCE = re.search(
    r"<script>(.*?)</script>",
    (ROOT / "src/web/acoustic_lab.html").read_text(),
    re.S,
).group(1)

HARNESS = r"""
const vm = require('node:vm');
const assert = require('node:assert/strict');
const input = JSON.parse(require('node:fs').readFileSync(0, 'utf8'));
const nodes = new Map();
class Element {
  constructor(tag = 'div') {
    this.tag = tag; this.children = []; this.handlers = {}; this.dataset = {};
    this.style = {}; this.value = ''; this.checked = false; this.disabled = false;
    this.hidden = false; this.textContent = ''; this.files = [];
    this.classList = {toggle() {}};
  }
  set id(value) { this._id = value; nodes.set(value, this); }
  get id() { return this._id; }
  append(...items) { this.children.push(...items); }
  replaceChildren(...items) { this.children = items; }
  setAttribute(key, value) { this[key] = value; }
  addEventListener(type, handler) { this.handlers[type] = handler; }
  async fire(type, extra = {}) {
    if (this.handlers[type]) return this.handlers[type]({target:this, preventDefault() {}, ...extra});
  }
  checkValidity() {
    return this.validity.valid;
  }
  get validity() {
    let badInput = false, rangeUnderflow = false, rangeOverflow = false, stepMismatch = false;
    if (this.type === 'number' && this.value !== '') {
      const num = Number(this.value);
      if (Number.isNaN(num)) badInput = true;
      if (this.min !== undefined && num < Number(this.min) - 1e-6) rangeUnderflow = true;
      if (this.max !== undefined && num > Number(this.max) + 1e-6) rangeOverflow = true;
      if (this.step && this.step !== 'any') {
        const step = Number(this.step);
        const remainder = Math.abs((num - (Number(this.min) || 0)) % step);
        const eps = 1e-6;
        if (remainder > eps && Math.abs(remainder - step) > eps) stepMismatch = true;
      }
    }
    const valid = !badInput && !rangeUnderflow && !rangeOverflow && !stepMismatch;
    return { valid, badInput, rangeUnderflow, rangeOverflow, stepMismatch };
  }
  reportValidity() {
    for (const node of nodes.values()) {
      if (!node.checkValidity()) return false;
    }
    return true;
  }
  click() { return this.fire('click'); }
  remove() {}
}
const document = {
  hidden:false, handlers:{}, body:new Element('body'),
  getElementById(id) {
    if (!nodes.has(id)) { const node = new Element(); node.id = id; }
    return nodes.get(id);
  },
  createElement(tag) { return new Element(tag); },
  addEventListener(type, handler) { this.handlers[type] = handler; }
};
let current = {
  status:'ok', revision:1, pending:false, error:null,
  applied:{threshold:0.38, enabled:true, score_mode:'max', gain_ref:null},
  saved:{threshold:0.38, enabled:true, score_mode:'max', gain_ref:null},
  defaults:{threshold:0.4, enabled:true, score_mode:'max', gain_ref:null},
  controls:[
    {key:'threshold', label:'Балл', kind:'number', min:0, max:0.7311, step:0.001, group:'kws'},
    {key:'enabled', label:'Включён', kind:'boolean', group:'activation'},
    {key:'score_mode', label:'Режим', kind:'select', group:'kws', options:[{value:'max',label:'Max'},{value:'avg',label:'Avg'}]},
    {key:'gain_ref', label:'RMS', kind:'optional_number', min:0.1,max:32768,step:0.1,group:'filters'}
  ],
  runtime:{ready:true,model_loaded:true,model_name:'fixture',pcm_rms:12,noise_floor:2,vad_level:45,last_score:0.5,last_avg_score:0.2,candidate_count:3,keyword_count:1,opened_count:1,suppressed_count:0,shadow_count:0,mode:'keyword'},
  capabilities:[{label:'PCM',value:'16 kHz',editable:false,reason:'Фиксировано'}]
};
let calls = []; let postError = null;
const clone = value => JSON.parse(JSON.stringify(value));
const context = {
  console, document, AbortController, Blob, URL, setTimeout, clearTimeout,
  setInterval() { return 1; }, clearInterval() {},
  window:{addEventListener() {},print() {}},
  fetch: async (url, options = {}) => {
    assert.equal(url, '/api/v1/acoustics');
    calls.push({method:options.method || 'GET',body:options.body});
    if (options.method === 'POST') {
      const request = JSON.parse(options.body);
      if (postError) return {ok:false,status:postError.status,json:async()=>clone(postError.body)};
      assert.equal(request.expected_revision, current.revision);
      current = {...current,revision:current.revision+1,applied:request.settings,saved:request.settings};
    }
    return {ok:true,status:200,json:async()=>clone(current)};
  }
};
vm.createContext(context);
vm.runInContext(input.source, context);
context.fixture = current;
const evaluate = expression => vm.runInContext(expression, context);
const settle = () => new Promise(resolve => setImmediate(resolve));
(async () => {
  if (input.case === 'profiles') {
    assert.equal(evaluate('validateProfile({schema:1,settings:fixture.applied},fixture.controls,fixture.defaults).threshold'), 0.38);
    assert.equal(evaluate('validateProfile({schema:1,settings:fixture.applied},fixture.controls,fixture.defaults).gain_ref'), null);
    assert.equal(evaluate('settingsEqual({a:0.28},{a:0.2800000011920929})'), true, 'f32 float representation differences must compare equal');
    assert.equal(evaluate('valuesEqual(0.32, 0.3199999928474426)'), true, 'f32 float values must compare equal');
    assert.equal(evaluate('valuesEqual(0.2, 0.20005, 1e-6)'), false, 'gain_ref step must not be swallowed');
    assert.equal(evaluate('settingsEqual({gain_ref:0.2},{gain_ref:0.20005})'), false, 'gain_ref settings diff must be detected');
    assert.equal(evaluate('validateSettings({threshold:0.7311000227928162, enabled:true, score_mode:"average", gain_ref:null}, fixture.controls, fixture.defaults).score_mode'), 'avg');
    assert.equal(evaluate('validateSettings({threshold:0.7311000227928162, enabled:true, score_mode:"average", gain_ref:null}, fixture.controls, fixture.defaults).threshold'), 0.731);
    const invalid = [
      '{schema:2,settings:fixture.applied}',
      '{schema:1,settings:{...fixture.applied,threshold:0.9}}',
      '{schema:1,settings:{...fixture.applied,threshold:NaN}}',
      '{schema:1,settings:{...fixture.applied,threshold:Infinity}}',
      '{schema:1,settings:{...fixture.applied,enabled:"false"}}',
      '{schema:1,settings:{...fixture.applied,score_mode:"unknown"}}',
      '{schema:1,settings:{...fixture.applied,token:"do-not-import"}}',
      '{schema:1,settings:{threshold:0.4}}',
      '{schema:1,settings:fixture.applied,credentials:{}}'
    ];
    for (const profile of invalid) assert.throws(() => evaluate(`validateProfile(${profile},fixture.controls,fixture.defaults)`));
    assert.equal(calls.length, 0, 'Profile validation must not access HTTP');
  } else {
    context.initAcousticLab(); await settle();
    const get = id => document.getElementById(id);
    const edit = async value => { get('setting-threshold').value = String(value); await get('setting-threshold').fire('input'); };
    assert.equal(Number(get('setting-threshold').value), 0.38);
    assert.equal(get('apply-btn').disabled, true);
    if (input.case === 'draft') {
      await edit(0.42);
      assert.equal(get('apply-btn').disabled, false);
      current = {...current,revision:2,applied:{...current.applied,threshold:0.44},saved:{...current.saved,threshold:0.44}};
      document.handlers.visibilitychange(); await settle();
      assert.equal(Number(get('setting-threshold').value), 0.42, 'Polling must preserve a dirty form');
      assert.equal(get('conflict').hidden, false);
      assert.equal(get('apply-btn').disabled, true, 'Conflicting revisions may not overwrite a profile');
      await get('defaults-btn').fire('click');
      assert.equal(Number(get('setting-threshold').value), 0.4);
      assert.equal(calls.filter(call => call.method === 'POST').length, 0, 'Defaults must not auto-apply');
      await get('revert-btn').fire('click');
      assert.equal(Number(get('setting-threshold').value), 0.44);
      assert.equal(get('conflict').hidden, true);
      const profile = {schema:1,settings:{...current.applied,threshold:0.46}};
      get('profile-file').files = [{size:100,text:async()=>JSON.stringify(profile)}];
      await get('profile-file').fire('change');
      assert.equal(Number(get('setting-threshold').value), 0.46);
      assert.equal(calls.filter(call => call.method === 'POST').length, 0, 'Import must not auto-apply');
      await get('settings-form').fire('submit');
      const request = JSON.parse(calls.find(call => call.method === 'POST').body);
      assert.equal(request.expected_revision, 2);
      assert.equal(request.settings.threshold, 0.46);
      assert.equal(get('apply-btn').disabled, true);
      assert.match(get('message').textContent, /сохранён и применён/);
    } else if (input.case === 'failure') {
      await edit(0.42);
      postError = {status:503,body:{error:'Аудиоцикл не подтвердил применение'}};
      await get('settings-form').fire('submit');
      assert.equal(Number(get('setting-threshold').value), 0.42);
      assert.equal(get('message').dataset.tone, 'bad');
      assert.match(get('message').textContent, /не подтвердил/);
      assert.equal(get('apply-btn').disabled, false, 'User may retry after a failed apply');
      assert.equal(current.applied.threshold, 0.38);
    } else if (input.case === 'all_33_controls') {
      current.controls = input.controls;
      current.defaults = input.defaults;
      current.applied = clone(input.defaults);
      current.saved = clone(input.defaults);
      context.initAcousticLab(); await settle();
      const get = id => document.getElementById(id);
      assert.equal(input.controls.length, 33, 'Exactly 33 controls expected');
      for (const ctrl of input.controls) {
        const elNode = get(`setting-${ctrl.key}`);
        assert.ok(elNode, `Control element setting-${ctrl.key} must be created`);
        if (ctrl.kind === 'number') {
          assert.equal(elNode.type, 'number');
          assert.equal(elNode.checkValidity(), true, `Default value of ${ctrl.key} must be valid`);
          assert.equal(elNode.validity.valid, true);

          // Test underflow
          const originalVal = elNode.value;
          elNode.value = String(Number(ctrl.min) - 10);
          assert.equal(elNode.validity.rangeUnderflow, true, `${ctrl.key} should report rangeUnderflow`);
          assert.equal(elNode.checkValidity(), false);
          assert.equal(get('settings-form').reportValidity(), false, 'Form reportValidity must fail when field is underflow');

          // Test overflow
          elNode.value = String(Number(ctrl.max) + 10);
          assert.equal(elNode.validity.rangeOverflow, true, `${ctrl.key} should report rangeOverflow`);
          assert.equal(elNode.checkValidity(), false);
          assert.equal(get('settings-form').reportValidity(), false, 'Form reportValidity must fail when field is overflow');

          // Restore valid value
          elNode.value = originalVal;
          assert.equal(elNode.checkValidity(), true);
        }
      }
      assert.equal(get('settings-form').reportValidity(), true, 'Form reportValidity must pass when all 33 fields valid');
      // Verify edit & submit roundtrip of all 33 fields
      get('setting-threshold').value = '0.55';
      await get('setting-threshold').fire('input');
      assert.equal(get('apply-btn').disabled, false);
      await get('settings-form').fire('submit');
      const req = JSON.parse(calls.find(c => c.method === 'POST').body);
      assert.equal(req.settings.threshold, 0.55);
      assert.equal(Object.keys(req.settings).length, 33, 'All 33 fields must be submitted');
    } else throw new Error('Unknown case');
  }
})().catch(error => { console.error(error); process.exitCode = 1; });
"""


@unittest.skipUnless(NODE, "Node.js is required for offline browser-logic fixtures")
class AcousticUiTests(unittest.TestCase):
    def run_case(self, case, extra=None):
        payload = {"source": SOURCE, "case": case}
        if extra:
            payload.update(extra)
        result = subprocess.run(
            [NODE, "-e", HARNESS],
            input=json.dumps(payload),
            text=True,
            capture_output=True,
            timeout=10,
            check=False,
        )
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_profile_validation_rejects_malformed_or_unrelated_data(self):
        self.run_case("profiles")

    def test_dirty_polling_conflict_import_and_confirmed_apply(self):
        self.run_case("draft")

    def test_failed_apply_keeps_editable_draft_and_does_not_claim_success(self):
        self.run_case("failure")

    def test_all_33_controls_validity_and_report_validity(self):
        controls_33 = [
            {"key": "enabled", "label": "Распознавание", "kind": "boolean", "group": "activation"},
            {"key": "shadow_mode", "label": "Лабораторный", "kind": "boolean", "group": "activation"},
            {"key": "threshold", "label": "Основной порог", "kind": "number", "min": 0.0, "max": 0.7311, "step": 0.001, "group": "kws"},
            {"key": "avg_threshold", "label": "Порог усреднённой модели", "kind": "number", "min": 0.0, "max": 0.7311, "step": 0.001, "group": "kws"},
            {"key": "min_scores", "label": "Подтверждения", "kind": "number", "min": 1, "max": 100, "step": 1, "group": "kws"},
            {"key": "eager", "label": "Раннее срабатывание", "kind": "boolean", "group": "kws"},
            {"key": "score_mode", "label": "Объединение", "kind": "select", "group": "kws", "options": [{"value": "max", "label": "Max"}, {"value": "avg", "label": "Avg"}]},
            {"key": "score_ref", "label": "Масштаб балла", "kind": "number", "min": 0.01, "max": 2.0, "step": 0.01, "group": "kws"},
            {"key": "dtw_band_size", "label": "Полоса DTW", "kind": "number", "min": 1, "max": 32, "step": 1, "group": "kws"},
            {"key": "gain_normalizer", "label": "Нормализация KWS", "kind": "boolean", "group": "filters"},
            {"key": "gain_min", "label": "Минимум усиления", "kind": "number", "min": 0.1, "max": 20.0, "step": 0.1, "group": "filters"},
            {"key": "gain_max", "label": "Максимум усиления", "kind": "number", "min": 0.1, "max": 20.0, "step": 0.1, "group": "filters"},
            {"key": "gain_ref", "label": "Опорный RMS", "kind": "optional_number", "min": 0.00001, "max": 1.0, "step": 0.00001, "group": "filters"},
            {"key": "band_pass", "label": "Полосовой фильтр", "kind": "boolean", "group": "filters"},
            {"key": "band_low_hz", "label": "Нижняя частота", "kind": "number", "min": 20, "max": 7900, "step": 10, "group": "filters"},
            {"key": "band_high_hz", "label": "Верхняя частота", "kind": "number", "min": 20, "max": 7900, "step": 10, "group": "filters"},
            {"key": "mic_gain", "label": "Цифровой вход", "kind": "number", "min": 0.1, "max": 20.0, "step": 0.1, "group": "audio"},
            {"key": "vad_threshold", "label": "Порог VAD", "kind": "number", "min": 1, "max": 1000, "step": 1, "group": "vad"},
            {"key": "vad_noise_adaptation", "label": "Адаптация к шуму", "kind": "boolean", "group": "vad"},
            {"key": "vad_noise_multiplier", "label": "Множитель шума", "kind": "number", "min": 1, "max": 10, "step": 0.05, "group": "vad"},
            {"key": "vad_noise_offset", "label": "Запас к шуму", "kind": "number", "min": 0, "max": 1000, "step": 1, "group": "vad"},
            {"key": "vad_rise_alpha", "label": "Рост шума", "kind": "number", "min": 0.001, "max": 1, "step": 0.001, "group": "vad"},
            {"key": "vad_fall_alpha", "label": "Спад шума", "kind": "number", "min": 0.001, "max": 1, "step": 0.001, "group": "vad"},
            {"key": "vad_onset_frames", "label": "Кадры старта", "kind": "number", "min": 1, "max": 100, "step": 1, "group": "vad"},
            {"key": "pre_roll_ms", "label": "Предзапись", "kind": "number", "min": 0, "max": 2000, "step": 10, "group": "timing"},
            {"key": "initial_silence_timeout_ms", "label": "Ожидание фразы", "kind": "number", "min": 500, "max": 15000, "step": 100, "group": "timing"},
            {"key": "silence_timeout_ms", "label": "Пауза фразы", "kind": "number", "min": 200, "max": 10000, "step": 100, "group": "timing"},
            {"key": "max_utterance_secs", "label": "Макс реплика", "kind": "number", "min": 3, "max": 60, "step": 1, "group": "timing"},
            {"key": "manual_activation_secs", "label": "Ручное окно", "kind": "number", "min": 1, "max": 30, "step": 1, "group": "timing"},
            {"key": "followup_timeout_secs", "label": "Продолжение", "kind": "number", "min": 0, "max": 15, "step": 1, "group": "timing"},
            {"key": "echo_tail_ms", "label": "Защита хвоста", "kind": "number", "min": 0, "max": 5000, "step": 50, "group": "timing"},
            {"key": "startup_suppression_ms", "label": "Прогрев PCM", "kind": "number", "min": 0, "max": 10000, "step": 100, "group": "timing"},
            {"key": "wake_chime", "label": "Сигнал вызова", "kind": "boolean", "group": "feedback"},
        ]
        defaults_33 = {
            "enabled": True, "shadow_mode": False, "threshold": 0.42, "avg_threshold": 0.22,
            "min_scores": 1, "eager": False, "score_mode": "avg", "score_ref": 1.0,
            "dtw_band_size": 16, "gain_normalizer": True, "gain_min": 0.5, "gain_max": 2.0,
            "gain_ref": None, "band_pass": False, "band_low_hz": 200, "band_high_hz": 4000,
            "mic_gain": 1.0, "vad_threshold": 20, "vad_noise_adaptation": True,
            "vad_noise_multiplier": 2.0, "vad_noise_offset": 5, "vad_rise_alpha": 0.05,
            "vad_fall_alpha": 0.01, "vad_onset_frames": 2, "pre_roll_ms": 500,
            "initial_silence_timeout_ms": 3500, "silence_timeout_ms": 800,
            "max_utterance_secs": 15, "manual_activation_secs": 10, "followup_timeout_secs": 0,
            "echo_tail_ms": 400, "startup_suppression_ms": 200, "wake_chime": True,
        }
        self.run_case("all_33_controls", extra={"controls": controls_33, "defaults": defaults_33})


if __name__ == "__main__":
    unittest.main()
