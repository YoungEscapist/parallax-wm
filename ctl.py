#!/usr/bin/env python3
"""Одна команда управляющему сокету dawn. Использование: ctl.py '<строка>'"""
import socket, sys, os

путь = os.environ.get("DAWN_CTL_SOCKET", "/tmp/dawn-harness/run/dawn-wayland-1.ctl")
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
