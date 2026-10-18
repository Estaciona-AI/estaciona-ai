# Licenciamento e detector

## Código do Estaciona AI

O código próprio desta revisão passa a Apache-2.0, por determinação do titular.
A licença permite uso comercial, modificações e derivados com código fechado,
com concessão expressa de patentes. Distribuições devem respeitar o texto de
LICENSE, manter as atribuições pertinentes e identificar modificações.

Apache-2.0 continua sendo uma licença de código aberto: terceiros também podem
usar o código publicado. Ela não torna este repositório privado nem reserva
exclusividade comercial. A alteração não revoga os direitos concedidos sob
AGPL nas revisões anteriores, não reescreve o histórico e não relicencia
bibliotecas, datasets, vídeos, marcas ou pesos de terceiros.

Os headers SPDX foram aplicados aos arquivos próprios Rust e Python, inclusive
testes. As migrations históricas não foram editadas: seus checksums precisam
continuar válidos nas instalações existentes. Arquivos gerados e configurações
sem header individual seguem a licença da raiz, salvo atribuição específica.

## Detector escolhido e evidência disponível

**RT-DETR HGNetv2-L**, exportado da implementação oficial Paddle, a 640×640.
No catálogo local `benchmark_stress_25.json`, de 17/10/2026, ele apresentou
4,21 FPS por câmera, 105,2 FPS agregados e 6.840 MB de VRAM em 25 instâncias
na RTX 5060 Ti 16 GB. O ensaio durou oito segundos e usou TensorRT FP16 por
meio da Ultralytics; essas medições não certificam o runtime novo nem a precisão
de ocupação. O catálogo classificava YOLOv10-X como Apache, mas sua implementação
oficial é AGPL: essa alternativa foi descartada na revisão de licenças.

A migração usa ONNX Runtime 1.31.0 (MIT), sem `ultralytics`, sem importação de
`.pt` e sem reaproveitamento dos `.onnx`/`.engine` gerados pela Ultralytics.
O PyTorch continua no forecast; foi removido somente das dependências da visão.
O algoritmo de ocupação, os polígonos, o debounce e os eventos permanecem.
O limiar de confiança foi preservado; a precisão e a capacidade simultânea
precisam ser novamente medidas para esta combinação de pesos e runtime.

O modelo não é baixado ao iniciar a visão. O exportador gera o ONNX, uma cópia
da licença upstream e um arquivo de procedência com hashes. Usa os pesos
oficiais abaixo, verifica seu SHA-256 e fixa o commit da implementação:

- Implementação: https://github.com/lyuwenyu/RT-DETR
- Commit: `df2a96c8c3a098567a1149bf0248da6efe6d495d`
- Pesos: https://bj.bcebos.com/v1/paddledet/models/rtdetr_hgnetv2_l_6x_coco.pdparams
- SHA-256: `3bf9283e00183818e53840ea477f3c36f9accb7b5eb0f62a3ece4bee94cdb493`
- Licença upstream: https://github.com/lyuwenyu/RT-DETR/blob/df2a96c8c3a098567a1149bf0248da6efe6d495d/LICENSE

O exportador usa um ambiente separado Python 3.11 / Paddle 2.6.2 /
Paddle2ONNX 1.0.8, com lock próprio. O runtime do produto mantém Python 3.14.
Não é necessário instalar o exportador na implantação que recebe o ONNX pronto.

```sh
cd vision
uv run --project export --locked python tools/export_rtdetr.py
uv sync --locked --extra cuda
VISION_MODEL_PATH=models/rtdetr-hgnetv2-l.onnx VISION_DEVICE=cuda uv run --locked python src/client.py
```

Mantenha as demais variáveis de instalação, vídeo/RTSP e credenciais já usadas.
O exportador recusa sobrescrever artefatos existentes. Distribua também o arquivo
`.provenance.json` e a licença `.LICENSE` gerados. Para CPU, selecione
`VISION_DEVICE=cpu`; não há fallback silencioso quando CUDA foi solicitado.
Não use os artefatos antigos apenas renomeando sua extensão. Modelos customizados
configurados pelo operador exigem conferência própria da licença/procedência.

## Dependências e distribuição

A licença da raiz não substitui as licenças das dependências. As bibliotecas
principais seguem licenças permissivas: ONNX Runtime (MIT), OpenCV (Apache-2.0),
NumPy/Pandas/scikit-learn/PyTorch (BSD), LightGBM (MIT), e o ecossistema Rust
com opções MIT/Apache e outras permissivas. A enumeração de metadados Cargo
foi conferida sem encontrar dependência Rust exclusivamente GPL/AGPL; licenças
alternativas devem ser respeitadas conforme a opção escolhida pelo distribuidor.

`psycopg2-binary` mantém LGPL com exceção de ligação; as bibliotecas binárias
empacotadas têm suas próprias atribuições. Os wheels OpenCV também podem incluir
FFmpeg LGPL e bibliotecas Qt LGPL. Isso não equivale a relicenciá-las como Apache:
a distribuição deve preservar avisos, licenças e condições aplicáveis, incluindo
substituição/religação quando exigida. Pacotes do sistema operacional são
componentes separados e mantêm seus avisos.

O extra `cuda` instala bibliotecas NVIDIA sob os termos de redistribuição da
NVIDIA, não sob Apache/MIT. O driver continua sendo requisito do host. TensorRT
não é uma dependência deste runtime; os engines antigos permanecem fora dele.

As cópias em LICENSES/ cobrem os componentes diretamente integrados nesta troca;
não são um inventário jurídico exaustivo do sistema operacional ou dos wheels.
Ao distribuir binários/container, mantenha também as licenças e avisos completos
presentes nos pacotes efetivamente enviados. A imagem do servidor inclui os
arquivos de licença do projeto em `/usr/share/licenses/estaciona-ai/`; os
modelos locais são excluídos do contexto Docker e provisionados separadamente. O gate `scripts/check_licenses.py`
verifica consistência dos metadados/headers e bloqueia a reintrodução dos pacotes
Ultralytics no lock da visão; não certifica licenças transitivas ou modelos
customizados. Marcas e identidade visual não são concedidas pela Apache-2.0.

Referências: https://www.apache.org/licenses/LICENSE-2.0 e
https://github.com/microsoft/onnxruntime/blob/v1.31.0/LICENSE.
