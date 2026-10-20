// The container's health check: asks the host on this machine whether it can take work.
import http from 'node:http';

const bound = process.env.HOST;
const host = !bound || bound === '0.0.0.0' ? '127.0.0.1' : bound === '::' ? '::1' : bound;
const request = http.get({ host, port: Number(process.env.PORT || 8090), path: '/readyz', timeout: 3000 }, (reply) => {
  reply.resume();
  process.exit(reply.statusCode === 200 ? 0 : 1);
});
request.on('timeout', () => request.destroy());
request.on('error', () => process.exit(1));
