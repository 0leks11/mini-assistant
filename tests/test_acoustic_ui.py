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
  reportValidity() { return true; }
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
    } else throw new Error('Unknown case');
  }
})().catch(error => { console.error(error); process.exitCode = 1; });
"""


@unittest.skipUnless(NODE, "Node.js is required for offline browser-logic fixtures")
class AcousticUiTests(unittest.TestCase):
    def run_case(self, case):
        result = subprocess.run(
            [NODE, "-e", HARNESS],
            input=json.dumps({"source": SOURCE, "case": case}),
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


if __name__ == "__main__":
    unittest.main()
