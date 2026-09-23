#!/usr/bin/env python3
"""Exercise real TLS + WebSocket + Noise via a local TLS-terminating proxy.

No public service, model call, certificate bypass, or reusable key material.
The production TLS reverse-proxy configuration remains an operational example,
not something this isolated integration test can certify.
"""
import argparse
import os
from pathlib import Path
import select
import socket
import ssl
import subprocess
import sys
import tempfile
import threading

from e2e import ManagedProcess, Session, Suite, pick_port, require, wait_until


def openssl(*args):
    result = subprocess.run(['openssl', *map(str, args)], stdout=subprocess.PIPE,
                            stderr=subprocess.PIPE, text=True, timeout=15)
    require(result.returncode == 0, f'local test certificate generation failed: {result.stderr[-1200:]}')


def certificates(root):
    ca_key, ca_cert = root / 'ca.key', root / 'ca.crt'
    key, csr, cert, ext = root / 'server.key', root / 'server.csr', root / 'server.crt', root / 'server.ext'
    openssl('genrsa', '-out', ca_key, '2048')
    openssl('req', '-x509', '-new', '-key', ca_key, '-sha256', '-days', '1', '-out', ca_cert,
            '-subj', '/CN=agent-tunnel-ephemeral-test-CA', '-addext', 'basicConstraints=critical,CA:TRUE')
    openssl('req', '-newkey', 'rsa:2048', '-nodes', '-keyout', key, '-out', csr,
            '-subj', '/CN=127.0.0.1')
    ext.write_text('subjectAltName=IP:127.0.0.1\nbasicConstraints=CA:FALSE\n'
                   'extendedKeyUsage=serverAuth\nkeyUsage=digitalSignature,keyEncipherment\n')
    openssl('x509', '-req', '-in', csr, '-CA', ca_cert, '-CAkey', ca_key, '-CAcreateserial',
            '-out', cert, '-days', '1', '-sha256', '-extfile', ext)
    # The second server certificate deliberately has NO 127.0.0.1 SAN.
    # With the TCP IP override, only preservation of the URL hostname for
    # SNI/certificate verification can make its handshake succeed.
    pinned_key, pinned_csr, pinned_cert, pinned_ext = (
        root / 'pinned.key', root / 'pinned.csr', root / 'pinned.crt', root / 'pinned.ext')
    openssl('req', '-newkey', 'rsa:2048', '-nodes', '-keyout', pinned_key, '-out', pinned_csr,
            '-subj', '/CN=relay.agent-tunnel.invalid')
    pinned_ext.write_text('subjectAltName=DNS:relay.agent-tunnel.invalid\nbasicConstraints=CA:FALSE\n'
                          'extendedKeyUsage=serverAuth\nkeyUsage=digitalSignature,keyEncipherment\n')
    openssl('x509', '-req', '-in', pinned_csr, '-CA', ca_cert, '-CAkey', ca_key,
            '-out', pinned_cert, '-days', '1', '-sha256', '-extfile', pinned_ext)
    return ca_cert, cert, key, pinned_cert, pinned_key


class TlsIngress:
    def __init__(self, cert, key, port, relay_port):
        self.ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        self.ctx.load_cert_chain(cert, key)
        self.relay_port = relay_port
        self.server = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        self.server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self.server.bind(('127.0.0.1', port))
        self.server.listen(16)
        self.server.settimeout(.25)
        self.stop_event = threading.Event()
        self.children = []
        self.thread = threading.Thread(target=self.run, daemon=True)
        self.thread.start()

    def run(self):
        while not self.stop_event.is_set():
            try:
                client, _ = self.server.accept()
            except socket.timeout:
                continue
            except OSError:
                break
            worker = threading.Thread(target=self.pipe, args=(client,), daemon=True)
            self.children.append(worker)
            worker.start()

    def pipe(self, client):
        try:
            with self.ctx.wrap_socket(client, server_side=True) as front:
                with socket.create_connection(('127.0.0.1', self.relay_port), timeout=5) as back:
                    back.settimeout(5)
                    front.settimeout(5)
                    while not self.stop_event.is_set():
                        ready, _, _ = select.select([front, back], [], [], .2)
                        for src in ready:
                            try: chunk = src.recv(256 * 1024)
                            except (OSError, ssl.SSLError): return
                            if not chunk: return
                            (back if src is front else front).sendall(chunk)
        except (OSError, ssl.SSLError):
            try: client.close()
            except OSError: pass

    def close(self):
        self.stop_event.set()
        self.server.close()
        self.thread.join(3)
        for child in self.children:
            child.join(3)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--binary', required=True)
    args = parser.parse_args()
    binary = Path(args.binary).resolve()
    require(binary.is_file() and os.access(binary, os.X_OK), f'binary unavailable: {binary}')
    with tempfile.TemporaryDirectory(prefix='agent-tunnel-wss-') as tmp:
        root = Path(tmp)
        ca, cert, key, pinned_cert, pinned_key = certificates(root)
        unrelated_key, unrelated_ca = root / 'unrelated-ca.key', root / 'unrelated-ca.crt'
        openssl('genrsa', '-out', unrelated_key, '2048')
        openssl('req', '-x509', '-new', '-key', unrelated_key, '-sha256', '-days', '1',
                '-out', unrelated_ca, '-subj', '/CN=unrelated-test-CA',
                '-addext', 'basicConstraints=critical,CA:TRUE')
        relay_port, tls_port = pick_port(), pick_port()
        while tls_port == relay_port:
            tls_port = pick_port()
        pinned_port = pick_port()
        while pinned_port in (relay_port, tls_port):
            pinned_port = pick_port()
        session = Session(binary, root / 'session', 'ephemeral-wss', f'wss://127.0.0.1:{tls_port}/', 10)
        pinned = Session(binary, root / 'pinned', 'pinned-wss',
                         f'wss://relay.agent-tunnel.invalid:{pinned_port}/', 10)
        untrusted = Session(binary, root / 'untrusted', 'untrusted-wss',
                            f'wss://relay.agent-tunnel.invalid:{pinned_port}/', 10)
        session.initialize()
        pinned.initialize()
        untrusted.initialize()
        relay = ManagedProcess([str(binary), 'relay', '--listen', f'127.0.0.1:{relay_port}',
                                '--session-file', str(session.relay_file),
                                '--session-file', str(pinned.relay_file),
                                '--session-file', str(untrusted.relay_file)], 'tls-smoke-relay')
        ingress = None
        pinned_ingress = None
        previous = os.environ.get('SSL_CERT_FILE')
        previous_ip = os.environ.pop('AGENT_TUNNEL_CONNECT_IP', None)
        try:
            relay.wait_for_log('relay listening on', 5)
            ingress = TlsIngress(cert, key, tls_port, relay_port)
            pinned_ingress = TlsIngress(pinned_cert, pinned_key, pinned_port, relay_port)
            os.environ['SSL_CERT_FILE'] = str(ca)
            session.start()
            info = session.info()
            require(info['end_to_end_encrypted'] is True, 'Noise did not authenticate end-to-end')
            suite = Suite(binary, 10)
            marker = root/'safe-wss-output'
            code = f'from pathlib import Path; Path({str(marker)!r}).write_text("tls-wss-noise-ok")'
            reply = suite.register_reply(session.exec('tls-wss-smoke', [sys.executable, '-c', code], cwd=root,
                                                      timeout_ms=5000), 'TLS exec')
            state, _ = suite.read_to_terminal(session, reply['job_id'], timeout=10)
            require(state['exit_code'] == 0 and marker.read_text() == 'tls-wss-noise-ok',
                    'TLS/WSS/Noise remote command did not execute exactly once')
            # An explicit connection IP must not replace the configured Host/SNI
            # or bypass certificate validation. Use a distinct session because
            # Relay binds each role incarnation for the lifetime of a session.
            os.environ['AGENT_TUNNEL_CONNECT_IP'] = '127.0.0.1'
            pinned.start()
            pinned_info = pinned.info()
            require(pinned_info['end_to_end_encrypted'] is True, 'IP override lost Noise auth')
            other = root / 'pinned-wss-output'
            reply = suite.register_reply(pinned.exec('pinned-ip-test', [sys.executable, '-c',
                f'from pathlib import Path; Path({str(other)!r}).write_text("tls-hostname-still-verified")'],
                cwd=root, timeout_ms=5000), 'pinned IP TLS exec')
            state, _ = suite.read_to_terminal(pinned, reply['job_id'], timeout=10)
            require(state['exit_code'] == 0 and other.read_text() == 'tls-hostname-still-verified',
                    'explicit connection IP did not preserve TLS/WSS/Noise execution')
            # Swapping in an unrelated CA must not silently disable TLS
            # validation, even when the network destination is pinned by IP.
            os.environ['SSL_CERT_FILE'] = str(unrelated_ca)
            untrusted.start_connector()
            untrusted.start_controller(wait_for_connection=False)
            def denied():
                _, response = untrusted.try_info()
                return response and response.get('error', {}).get('code') == 'TARGET_OFFLINE'
            wait_until(denied, 5, 'untrusted test CA rejected before Noise channel')
            wait_until(lambda: untrusted.controller is not None and
                       'certificate' in untrusted.controller.logs().lower(),
                       5, 'Rustls certificate verification actually rejected the wrong CA')
            require('end-to-end channel authenticated' not in untrusted.controller.logs(),
                    'wrong CA unexpectedly established a controller channel')
            print('PASS: TLS trusted CA, WSS, Noise, unresolvable hostname via explicit IP/SNI; wrong CA fails closed')
        finally:
            untrusted.stop()
            pinned.stop()
            session.stop()
            if ingress: ingress.close()
            if pinned_ingress: pinned_ingress.close()
            relay.terminate()
            if previous is None: os.environ.pop('SSL_CERT_FILE', None)
            else: os.environ['SSL_CERT_FILE'] = previous
            if previous_ip is not None: os.environ['AGENT_TUNNEL_CONNECT_IP'] = previous_ip

if __name__ == '__main__': main()
