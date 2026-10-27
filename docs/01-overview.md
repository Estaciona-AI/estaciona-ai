# 1. Overview

## Abstract

Estaciona AI is a parking-management system that infers the occupancy of
individual parking spots from video, delivers those observations reliably from
the site to a backend, and serves current state, reservations, in-lot routing
and short-term occupancy forecasts to client applications. This repository
contains the *core*: the backend server, the edge gateway, the vision service,
the forecast worker, the wire contracts, the infrastructure definitions and
supporting tools. Release `v0.9.0` (2026-10-23) is the first pilot release:
it is intended for supervised pilot installations and has not yet been
validated at its target capacity or with a full set of real cameras
([Evidence register](06-evidence-register.md)).

## 1.1 Problem statement

Drivers on a campus or similar site spend time searching for free spots, and
operators lack reliable, current and historical occupancy data. A practical
system must (i) detect occupancy from existing cameras without per-spot
sensors, (ii) remain correct when the site network is intermittent,
(iii) protect the personal data of the people it serves, and (iv) give
forecasts whose quality is measured rather than asserted.

## 1.2 Scope

In scope: the components listed in §1.3, their contracts, their deployment
definitions and their tests.

Out of scope: client applications (the web hub and installation panels live in
separate repositories and consume the published contracts), customer site data,
and production operations of any particular installation.

## 1.3 Components

| Component | Directory | Language and main libraries | Responsibility |
|---|---|---|---|
| Server | `server/` | Rust (edition 2024), Axum, SQLx, PostgreSQL | REST API, WebSockets, reservations, routing, identity integration, durable event application |
| Gateway | `gateway/` | Python 3.14, `websockets`, SQLite | Receives local vision events, persists them, and delivers them to the server with acknowledgements |
| Forecast worker | `gateway/ml/` | Python 3.14, LightGBM, pandas | Trains and serves 24-hour occupancy-rate forecasts from the installation history |
| Vision | `vision/` | Python 3.14, ONNX Runtime, OpenCV | Reads camera streams, detects vehicles, derives per-spot occupancy, keeps a durable local outbox |
| Model export | `vision/export/` | Python 3.11, PaddlePaddle, paddle2onnx | Reproducible export of the detector to ONNX with provenance |
| Contracts | `shared/` | JSON Schema 2020-12, pytest | Published wire schemas, fixtures and compatibility checks |
| Infrastructure | `infra/` | Docker Compose, PostgreSQL 15, Keycloak 26.8, Caddy | Per-installation stack and central identity provider |
| Tools | `tools/` | Python | Identity provisioning and a reproducible RTSP laboratory |

## 1.4 Deployment model

Each installation (one site of one organization) runs its own server process and
its own PostgreSQL database, bound to explicit organization, installation and
environment identifiers. Identity is central: one identity provider serves all
installations, each registered as a separate client. Multi-tenancy within one
backend and horizontal replicas are not implemented (`CHANGELOG.md`, *Known
limitations*).

## 1.5 Terminology

| Term | Meaning |
|---|---|
| Installation | One deployed site with its own server, database, maps, credentials and identity client |
| Input | One camera stream processed by the vision service |
| Spot | A parking space, defined by a polygon in a camera image and a node in the site map |
| Observation | A spot status (`free`, `occupied`) reported by vision with a timestamp; a spot without a recent observation is `unknown` |
| Edge | A producer that sends events to the server through `/ws/edge` (the gateway, the forecast worker) |
| Outbox | A durable local queue that keeps an event until its receiver acknowledges it |
| Site map | The installation's spots, parking lots, entrances and navigation paths |
| Pilot | A supervised installation used to validate the system before production use |
