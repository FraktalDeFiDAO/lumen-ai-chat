import { createServer } from 'node:net'

export async function reservePort() {
  const server = createServer()
  await new Promise((resolve, reject) => {
    server.once('error', reject)
    server.listen(0, '127.0.0.1', resolve)
  })
  const address = server.address()
  if (!address || typeof address === 'string') throw new Error('Unable to reserve a loopback test port')
  await new Promise((resolve, reject) => server.close(error => error ? reject(error) : resolve()))
  return address.port
}

export async function reservePorts(count) {
  const ports = new Set()
  while (ports.size < count) ports.add(await reservePort())
  return [...ports]
}
