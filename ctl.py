#!/usr/bin/env python3
"""Одна команда управляющему сокету parallax. Использование: ctl.py '<строка>'"""
import socket, sys, os

путь = os.environ.get("PLX_CTL_SOCKET", "/tmp/parallax-harness/run/plx-wayland-1.ctl")
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect(путь)
s.sendall((" ".join(sys.argv[1:]) + "\n").encode())
s.shutdown(socket.SHUT_WR)
данные = b""
while True:
    кусок = s.recv(65536)
    if not кусок:
        break
    данные += кусок
sys.stdout.write(данные.decode(errors="replace"))
