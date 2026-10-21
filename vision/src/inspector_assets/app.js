// SPDX-FileCopyrightText: 2026 Guilherme Pedroza
// SPDX-License-Identifier: Apache-2.0
'use strict';
const el = id => document.getElementById(id);
const labels = {starting:'Inicializando',waiting:'Aguardando captura',active:'Ativo',backpressure:'Entrega pendente',stopped:'Encerrado',failed:'Falha no worker',invalid:'Configuração ou mídia inválida',backoff:'Aguardando reinício'};
let paused = false;
let busy = false;
let displayed = null;
async function api(path) {
  const response = await fetch(path, {headers:{'X-Inspector-Request':'1'},cache:'no-store',signal:AbortSignal.timeout(2000)});
  if (!response.ok) throw new Error('inspector unavailable');
  return response.json();
}
function clear() {
  displayed = null;
  el('image').hidden = true;
  el('image').removeAttribute('src');
  el('empty').hidden = false;
  el('empty').textContent = 'Aguardando um frame analisado deste input…';
  for (const id of ['fps','age','latency','occupancy','pending','sequence']) el(id).textContent = '—';
  el('total').textContent = 'aguardando observação';
  el('spots').replaceChildren();
  el('observed').textContent = 'Nenhum frame recebido';
}
async function render(data) {
  el('status').textContent = labels[data.status] || 'Indisponível';
  el('model').textContent = data.model ? `${data.model} · ${data.device}` : '';
  const stale = data.stale || data.status !== 'active';
  el('diagnostic').textContent = `${labels[data.status] || 'Indisponível'}. ${stale ? 'Sem observação atual confirmada; não interpretar a imagem como estado ao vivo.' : 'Worker processando o input selecionado.'}`;
  if (data.preview_error) el('diagnostic').textContent = 'Falha no preview; o processamento de ocupação continua independente.';
  const frame = data.frame;
  if (!frame) { clear(); return; }
  function freshness() {
    document.querySelector('.preview').classList.toggle('stale', stale);
    el('age').textContent = `${(data.age_ms/1000).toFixed(1)} s`;
    el('freshness').textContent = stale ? 'Observação histórica · aguarde uma nova captura' : 'Preview amostrado · até 2 imagens/s';
  }
  // JPEG and metadata arrive in one atomic snapshot, never independently fetched.
  const key = `${frame.epoch}:${frame.sequence}:${frame.observed_at}`;
  if (displayed === key) { freshness(); return; }
  const image = new Image();
  image.src = `data:image/jpeg;base64,${frame.jpeg}`;
  await image.decode();
  if (paused || data.input_id !== el('input').value) return;
  // Commit decoded pixels and their metadata together in one browser task.
  image.id = 'image';
  image.alt = 'Último frame analisado com caixas e polígonos';
  el('image').replaceWith(image);
  displayed = key;
  freshness();
  el('empty').hidden = true;
  el('fps').textContent = frame.fps.toFixed(1);
  el('latency').textContent = `${frame.latency_ms.toFixed(0)} ms`;
  el('pending').textContent = String(frame.pending);
  const occupied = frame.spots.filter(s => s.status === 'occupied').length;
  el('occupancy').textContent = `${occupied} / ${frame.spots.length}`;
  el('total').textContent = 'ocupadas / cadastradas';
  el('sequence').textContent = `#${frame.sequence} · geração ${frame.epoch}`;
  el('observed').textContent = `${new Date(frame.observed_at).toLocaleString('pt-BR')} · captura`;
  el('spots').replaceChildren(...frame.spots.map((spot, index) => {
    const tag = document.createElement('span');
    tag.className = spot.status === 'occupied' ? 'spot occupied' : 'spot free';
    tag.textContent = `${index + 1} · ${spot.id} · ${spot.status === 'occupied' ? 'ocupada' : 'livre'}`;
    return tag;
  }));
}
async function poll() {
  if (busy || paused || document.hidden || !el('input').value) return;
  busy = true;
  const selected = el('input').value;
  try {
    const data = await api(`/api/frame?input=${encodeURIComponent(selected)}`);
    if (!paused && selected === el('input').value && data.input_id === selected) await render(data);
  } catch {
    el('status').textContent = 'Inspetor indisponível';
    el('diagnostic').textContent = 'Não foi possível consultar o runtime local. A imagem anterior não representa uma observação atual.';
    document.querySelector('.preview').classList.add('stale');
    el('freshness').textContent = 'Sem atualização · imagem histórica';
  } finally { busy = false; }
}
el('input').addEventListener('change', () => { clear(); poll(); });
el('pause').addEventListener('click', () => {
  paused = !paused;
  el('pause').textContent = paused ? 'Retomar preview' : 'Pausar preview';
  if (paused) {
    document.querySelector('.preview').classList.add('stale');
    el('freshness').textContent = 'Preview pausado · imagem histórica';
  } else { clear(); poll(); }
});
async function start() {
  try {
    const data = await api('/api/status');
    for (const input of data.inputs) {
      const option = document.createElement('option');
      option.value = input.id;
      option.textContent = input.id;
      el('input').append(option);
    }
    await poll();
    setInterval(poll, 500);
  } catch {
    el('status').textContent = 'Inspetor indisponível · recarregue a página';
  }
}
start();
