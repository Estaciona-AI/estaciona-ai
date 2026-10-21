# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

"""Browser-independent sequencing test; uses Node only when already installed."""

from pathlib import Path
import shutil
import subprocess

import pytest


@pytest.mark.skipif(
    shutil.which("node") is None, reason="optional JavaScript runtime not installed"
)
def test_image_is_decoded_before_metadata_changes_and_switched_input_is_discarded():
    script = Path(__file__).parents[1] / "src/inspector_assets/app.js"
    harness = r"""
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const nodes = new Map();
function element(id) {
  if (!nodes.has(id)) nodes.set(id, {textContent:'old', value:'camera-a', hidden:false,
    classList:{toggle(){},add(){}}, addEventListener(){}, replaceChildren(){},
    removeAttribute(){}, replaceWith(next){nodes.set(id,next);}});
  return nodes.get(id);
}
let finishDecode;
class Image {
  decode() {return new Promise(resolve => {finishDecode = resolve;});}
  replaceWith(next) {nodes.set('image',next);}
  removeAttribute() {}
}
const context = vm.createContext({Image, console, AbortSignal:{timeout(){}},
  document:{getElementById:element, querySelector:()=>element('preview'), createElement:()=>({})},
  fetch:()=>new Promise(()=>{}), setInterval(){}});
vm.runInContext(fs.readFileSync(process.argv[2], 'utf8'),context);
const data = {input_id:'camera-a',status:'active',stale:false,age_ms:50,frame:{
  epoch:1,sequence:9,observed_at:'2026-10-21T12:00:00Z',jpeg:'sample',
  fps:2.5,latency_ms:20,pending:0,spots:[]}};
context.data = data;
(async () => {
  const previous = element('image');
  const rendering = vm.runInContext('render(data)',context);
  assert.equal(element('sequence').textContent,'old','metadata advanced before JPEG decode');
  assert.equal(element('age').textContent,'old','age advanced before JPEG decode');
  assert.equal(element('image'), previous);
  finishDecode();
  await rendering;
  assert.equal(element('image').src,'data:image/jpeg;base64,sample');
  assert.match(element('sequence').textContent, /#9/);
  const displayed = element('image');
  data.frame.sequence = 10;
  const delayed = vm.runInContext('render(data)',context);
  element('input').value = 'camera-b';
  finishDecode();
  await delayed;
  assert.equal(element('image'),displayed,'old input replaced selected image');
  assert.match(element('sequence').textContent, /#9/);
})().catch(error => {console.error(error);process.exitCode=1;});
"""
    result = subprocess.run(
        ["node", "-", str(script)],
        input=harness,
        text=True,
        capture_output=True,
        timeout=5,
    )
    assert result.returncode == 0, result.stdout + result.stderr
