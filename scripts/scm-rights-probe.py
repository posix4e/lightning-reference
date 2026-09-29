#!/usr/bin/env python3
"""Bounded AF_UNIX/SCM_RIGHTS diagnostic only; no Lightning or Internet sockets."""
import argparse
from array import array
from collections import Counter
from contextlib import ExitStack
import fcntl
import json
import os
import socket
import sys
import time


def timeout_before(deadline, io_seconds):
    remaining = deadline - time.monotonic()
    if remaining <= 0:
        raise TimeoutError('global wall deadline reached')
    return min(remaining, io_seconds)


def observe(mode, deadline, io_seconds):
    result = {'mode': mode, 'phase': 'setup'}
    started = time.monotonic()
    try:
        with ExitStack() as stack:
            sender, receiver = socket.socketpair(socket.AF_UNIX, socket.SOCK_STREAM)
            client, server = socket.socketpair(socket.AF_UNIX, socket.SOCK_STREAM)
            for sock in (sender, receiver, client, server):
                stack.enter_context(sock)
            sender.settimeout(timeout_before(deadline, io_seconds))
            receiver.settimeout(timeout_before(deadline, io_seconds))
            result['flags_before_send'] = fcntl.fcntl(client.fileno(), fcntl.F_GETFL)
            result['phase'] = 'send_right'
            sent = sender.sendmsg([b'F'], [(socket.SOL_SOCKET, socket.SCM_RIGHTS,
                                          array('i', [client.fileno()]))])
            if sent != 1:
                raise RuntimeError('unexpected SCM_RIGHTS payload size')
            if mode == 'close_before_recv':
                client.close()
            result['phase'] = 'recv_right'
            data, ancdata, flags, _ = receiver.recvmsg(1, socket.CMSG_SPACE(array('i').itemsize))
            fds = array('i')
            for level, kind, raw in ancdata:
                if level == socket.SOL_SOCKET and kind == socket.SCM_RIGHTS:
                    fds.frombytes(raw[:len(raw) - len(raw) % fds.itemsize])
            if data != b'F' or len(fds) != 1 or flags & socket.MSG_CTRUNC:
                for fd in fds:
                    os.close(fd)
                raise RuntimeError('unexpected SCM_RIGHTS receipt')
            passed = stack.enter_context(socket.socket(fileno=fds[0]))
            result['flags_after_recv'] = fcntl.fcntl(passed.fileno(), fcntl.F_GETFL)
            if mode == 'retain_through_recv':
                client.close()
            # Bound every I/O; timeout flags are installed only after recording
            # the transferred descriptor's initial flags in both cases.
            server.settimeout(timeout_before(deadline, io_seconds))
            passed.settimeout(timeout_before(deadline, io_seconds))
            result['phase'] = 'write_request'
            passed.sendall(b'Q')
            result['phase'] = 'server_read'
            server.settimeout(timeout_before(deadline, io_seconds))
            request = server.recv(1)
            result['server_request_hex'] = request.hex()
            if request != b'Q':
                result['outcome'] = 'server_eof' if not request else 'unexpected_request'
            else:
                result['phase'] = 'server_reply'
                server.settimeout(timeout_before(deadline, io_seconds))
                server.sendall(b'A')
                result['phase'] = 'client_reply_read'
                passed.settimeout(timeout_before(deadline, io_seconds))
                reply = passed.recv(1)
                result['reply_hex'] = reply.hex()
                result['outcome'] = 'reply_ok' if reply == b'A' else ('client_eof' if not reply else 'unexpected_reply')
    except Exception as exc:
        result['outcome'] = 'timeout' if isinstance(exc, TimeoutError) else 'error'
        result['error_type'] = type(exc).__name__
        result['errno'] = getattr(exc, 'errno', None)
        result['error'] = str(exc)
    result['duration_seconds'] = time.monotonic() - started
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--iterations', type=int, default=20)
    parser.add_argument('--wall-seconds', type=float, default=10)
    parser.add_argument('--io-seconds', type=float, default=0.2)
    args = parser.parse_args()
    if not 1 <= args.iterations <= 64:
        parser.error('iterations must be 1..64 paired observations')
    if not 0 < args.wall_seconds <= 15 or not 0 < args.io_seconds <= 0.5:
        parser.error('wall bound must be <=15s; each I/O bound must be <=0.5s')
    started = time.monotonic()
    deadline = started + args.wall_seconds
    samples = []
    for iteration in range(args.iterations):
        modes = ('close_before_recv', 'retain_through_recv')
        if iteration % 2:
            modes = tuple(reversed(modes))
        for mode in modes:
            if time.monotonic() >= deadline:
                break
            sample = observe(mode, deadline, args.io_seconds)
            sample['iteration'] = iteration + 1
            samples.append(sample)
        if time.monotonic() >= deadline:
            break
    complete = len(samples) == 2 * args.iterations
    report = {'scope': 'harmless AF_UNIX socketpair diagnostic; same-process FD passing',
              'platform': sys.platform, 'uname': tuple(os.uname()),
              'limits': vars(args), 'duration_seconds': time.monotonic() - started,
              'complete': complete, 'samples': samples,
              'counts': {mode: dict(Counter(s['outcome'] for s in samples if s['mode'] == mode))
                         for mode in ('close_before_recv', 'retain_through_recv')},
              'kernel_cause_proven_for_original_ci': False}
    print(json.dumps(report, indent=2), flush=True)
    return 0 if complete else 2


if __name__ == '__main__':
    raise SystemExit(main())
