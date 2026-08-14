#!/usr/bin/env python3
"""Чтение лога прошивки с USB CDC-порта.

    ./tools/capture-log.py            # пока не прервут
    ./tools/capture-log.py 120        # 120 секунд
    ./tools/capture-log.py 60 /dev/cu.usbmodem1101

Зачем нужен, если есть `screen`: терминал держит порт монопольно и не годится
для скриптов и CI, а простое `cat` не переводит tty в raw и рвёт многобайтные
символы UTF-8.

Про диагностику. Скрипт всегда говорит, что происходит: если порт занят другой
программой (тот же `screen`), это видно сразу, а не выглядит как умершая
плата. Ровно на этом я однажды потерял время, решив, что зависла прошивка.
"""

import glob
import os
import select
import subprocess
import sys
import time

SILENCE_NOTICE_SECONDS = 15


def find_port(explicit):
    if explicit:
        return explicit
    ports = sorted(glob.glob("/dev/cu.usbmodem*"))
    return ports[0] if ports else None


def who_holds(port):
    """Кто держит порт. Пусто, если никто или lsof недоступен."""
    for path in (port, port.replace("/dev/cu.", "/dev/tty.")):
        try:
            out = subprocess.run(
                ["lsof", "-n", path], capture_output=True, text=True, timeout=5
            ).stdout.strip().splitlines()
        except (OSError, subprocess.SubprocessError):
            return ""
        if len(out) > 1:
            return out[1].split()[0]
    return ""


def make_raw(port):
    """Иначе терминальная дисциплина портит многобайтные символы."""
    subprocess.run(
        ["stty", "-f", port, "raw", "-echo", "-istrip"],
        capture_output=True,
        check=False,
    )


def main():
    duration = float(sys.argv[1]) if len(sys.argv) > 1 else float("inf")
    explicit = sys.argv[2] if len(sys.argv) > 2 else None

    deadline = time.time() + duration
    fd = None
    total = 0
    last_data = time.time()
    last_complaint = ""

    while time.time() < deadline:
        if fd is None:
            port = find_port(explicit)
            if port is None:
                complaint = "порт не найден — плата отключена?"
                if complaint != last_complaint:
                    print(f"[capture] {complaint}", file=sys.stderr, flush=True)
                    last_complaint = complaint
                time.sleep(1)
                continue
            try:
                fd = os.open(port, os.O_RDONLY | os.O_NONBLOCK)
            except OSError as e:
                holder = who_holds(port)
                complaint = f"{port}: {e.strerror}"
                if holder:
                    complaint += f" — порт держит {holder}"
                if complaint != last_complaint:
                    print(f"[capture] {complaint}", file=sys.stderr, flush=True)
                    last_complaint = complaint
                time.sleep(1)
                continue
            make_raw(port)
            print(f"[capture] читаю {port}", file=sys.stderr, flush=True)
            last_complaint = ""
            last_data = time.time()

        try:
            ready, _, _ = select.select([fd], [], [], 1.0)
            if ready:
                chunk = os.read(fd, 4096)
                if chunk:
                    total += len(chunk)
                    last_data = time.time()
                    sys.stdout.write(chunk.decode("utf-8", "replace"))
                    sys.stdout.flush()
        except BlockingIOError:
            pass
        except OSError as e:
            # Плату передёрнули или перепрошили — ждём и открываем заново.
            print(f"[capture] порт пропал ({e.strerror}), жду", file=sys.stderr, flush=True)
            os.close(fd)
            fd = None
            continue

        silence = time.time() - last_data
        if silence > SILENCE_NOTICE_SECONDS:
            print(
                f"[capture] тишина {int(silence)} с, принято {total} байт",
                file=sys.stderr,
                flush=True,
            )
            last_data = time.time()

    if fd is not None:
        os.close(fd)
    print(f"[capture] итого {total} байт", file=sys.stderr, flush=True)


if __name__ == "__main__":
    main()
