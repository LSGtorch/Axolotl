import crypto from 'node:crypto'
import fs from 'node:fs'

const [catalogPath, releasePath, tag] = process.argv.slice(2)
const serverUrl = process.env.UPDATE_SERVER_URL?.replace(/\/$/, '')
const webhookSecret = process.env.UPDATE_SERVER_WEBHOOK_SECRET

if (!catalogPath || !releasePath || !tag || !serverUrl || !webhookSecret) {
	throw new Error(
		'Usage: UPDATE_SERVER_URL=... UPDATE_SERVER_WEBHOOK_SECRET=... node scripts/axolotl/publish-update-server.mjs <catalog.json> <release.json> <version-tag>',
	)
}

const catalog = JSON.parse(fs.readFileSync(catalogPath, 'utf8'))
const release = JSON.parse(fs.readFileSync(releasePath, 'utf8'))
const version = tag.replace(/^v/, '')
if (catalog.version !== version || release.tag_name !== tag) {
	throw new Error('Release catalog metadata does not match the release tag')
}
if (!release.published_at) {
	throw new Error(`GitHub release ${tag} must be published before notifying the Update Server`)
}

const payload = JSON.stringify({
	event_id: `github-${tag}-${release.id ?? release.node_id ?? version}`,
	tag,
	version,
	channel: version.includes('-') ? 'beta' : 'release',
	release: {
		id: release.id,
		tag_name: release.tag_name,
		draft: release.draft,
		body: release.body ?? '',
		published_at: release.published_at,
		assets: release.assets,
	},
	catalog,
	force_update: process.env.UPDATE_SERVER_FORCE_UPDATE === 'true',
})
const timestamp = String(Math.floor(Date.now() / 1000))
const signature = crypto
	.createHmac('sha256', webhookSecret)
	.update(`${timestamp}.${payload}`)
	.digest('hex')
const response = await fetch(`${serverUrl}/api/webhook/release`, {
	method: 'POST',
	headers: {
		'Content-Type': 'application/json',
		'X-Webhook-Timestamp': timestamp,
		'X-Webhook-Signature': `sha256=${signature}`,
	},
	body: payload,
})
if (!response.ok) {
	throw new Error(`Update Server publish failed: ${response.status} ${await response.text()}`)
}
