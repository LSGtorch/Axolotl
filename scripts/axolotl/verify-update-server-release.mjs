const [tag] = process.argv.slice(2)
const serverUrl = process.env.UPDATE_SERVER_URL?.replace(/\/$/, '')

if (!tag || !serverUrl) {
	throw new Error(
		'Usage: UPDATE_SERVER_URL=... node scripts/axolotl/verify-update-server-release.mjs <version-tag>',
	)
}

const version = tag.replace(/^v/, '')
const channel = version.includes('-') ? 'beta' : 'release'
const response = await fetch(`${serverUrl}/latest`, {
	headers: {
		Accept: 'application/json',
		'X-Axolotl-Channel': channel,
		'X-Axolotl-Platform': 'windows-x86_64',
		'X-Axolotl-Version': '0.0.0',
	},
})
if (!response.ok) {
	throw new Error(`Update Server latest verification failed: ${response.status} ${await response.text()}`)
}
const latest = await response.json()
if (latest.version !== version || typeof latest.force_update !== 'boolean') {
	throw new Error(`Update Server returned an unexpected manifest for ${version}`)
}
const update = latest.platforms?.['windows-x86_64']
if (!update?.signature || !update?.url) {
	throw new Error('Update Server manifest is missing the Windows updater artifact')
}
const updateUrl = new URL(update.url)
if (updateUrl.protocol !== 'https:' || !updateUrl.pathname.startsWith(`/dist/${version}/`)) {
	throw new Error(`Unexpected Update Server updater URL: ${update.url}`)
}
const updateHead = await fetch(update.url, { method: 'HEAD' })
if (!updateHead.ok) {
	throw new Error(`Update Server updater artifact verification failed: ${update.url}`)
}
const downloads = await fetch(`${serverUrl}/api/downloads/${encodeURIComponent(version)}`)
if (!downloads.ok) {
	throw new Error(`Update Server download catalog verification failed: ${downloads.status}`)
}
const catalog = await downloads.json()
if (catalog.version !== version || !Array.isArray(catalog.downloads) || catalog.downloads.length === 0) {
	throw new Error(`Update Server download catalog is incomplete for ${version}`)
}
for (const artifact of catalog.downloads) {
	const url = new URL(artifact.url)
	if (url.protocol !== 'https:' || !url.pathname.startsWith(`/dist/${version}/`)) {
		throw new Error(`Unexpected complete package URL: ${artifact.url}`)
	}
	const head = await fetch(artifact.url, { method: 'HEAD' })
	if (!head.ok || Number(head.headers.get('content-length')) !== artifact.size) {
		throw new Error(`Complete package verification failed for ${artifact.filename}`)
	}
}
