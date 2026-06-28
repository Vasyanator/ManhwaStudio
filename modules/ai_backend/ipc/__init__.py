"""
File: modules/ai_backend/ipc/__init__.py

Purpose:
Package for the framed, multiplexed Rust <-> Python IPC protocol (v2).

This package contains the complete framed IPC implementation:
- `PROTOCOL.md`: authoritative wire specification.
- `protocol.py`: shared constants (method names, header keys, size guards).
- `framing.py`: pure wire codec — `read_frame`/`write_frame`/`encode_frame`.
- `dispatcher.py`: per-connection read loop, request routing, cancellation.
- `frame_server.py`: AF_UNIX server binding/lifecycle (`run_frame_server`).
- `events.py`: thread-safe event bus for server-initiated `event` frames.
- `registry.py`: method handler registry (`register`/`get_handler`).
- `handlers/`: one module per method group, each self-registering at import.

This is the sole IPC transport between the Rust frontend and the Python backend.
"""
