<script setup lang="ts">
import { EyeIcon, RefreshCwIcon, XIcon } from '@modrinth/assets'
import {
	ButtonStyled,
	Combobox,
	defineMessages,
	injectNotificationManager,
	NewButton as Button,
	NewModal,
	Toggle,
	useVIntl,
} from '@modrinth/ui'
import { getVersion } from '@tauri-apps/api/app'
import { invoke } from '@tauri-apps/api/core'
import { inject, nextTick, ref, watch } from 'vue'

import UpdateAnnouncementHistory from '@/components/ui/announcement/UpdateAnnouncementHistory.vue'
import {
	betaDatabaseExists,
	copyReleaseDatabaseToBeta,
	getUpdateChannel,
	getUpdatePreferences,
	setUpdatePreferences,
	setUpdateChannel,
	type UpdateChannel,
} from '@/helpers/settings.ts'
import { isDev, restartApp } from '@/helpers/utils.js'
import { type AppUpdateCheckResult, checkForAppUpdate } from '@/providers/app-update.ts'

import SettingsRow from './SettingsRow.vue'
import SettingsSection from './SettingsSection.vue'

const { formatMessage } = useVIntl()
const { handleError } = injectNotificationManager()
const selectedChannel = ref<UpdateChannel>(await getUpdateChannel())
const updatePreferences = ref(await getUpdatePreferences())
const checking = ref(false)
const checkResult = ref<AppUpdateCheckResult | 'failed' | 'portable' | null>(null)
const currentVersion = await getVersion()
const isDevEnvironment = await isDev()
const previewUpdateAnnouncement = inject<(version: string) => void>('previewUpdateAnnouncement')
const isPortable = ref(false)
const restartModal = ref<InstanceType<typeof NewModal>>()
const copyDatabaseModal = ref<InstanceType<typeof NewModal>>()
const pendingChannel = ref<UpdateChannel | null>(null)
let restoringChannelSelection = false

try {
	isPortable.value = await invoke('is_portable_mode')
} catch {
	// Best-effort check: fall back to non-portable when the command is unavailable.
}

const messages = defineMessages({
	title: {
		id: 'app.settings.updates.channel.title',
		defaultMessage: 'Update channel',
	},
	description: {
		id: 'app.settings.updates.channel.description',
		defaultMessage: 'Choose which launcher versions Axolotl receives.',
	},
	release: {
		id: 'app.settings.updates.channel.release',
		defaultMessage: 'Release',
	},
	beta: {
		id: 'app.settings.updates.channel.beta',
		defaultMessage: 'Beta',
	},
	check: {
		id: 'app.settings.updates.check',
		defaultMessage: 'Check for updates',
	},
	checking: {
		id: 'app.settings.updates.checking',
		defaultMessage: 'Checking for updates…',
	},
	available: {
		id: 'app.settings.updates.available',
		defaultMessage: 'An update is available.',
	},
	upToDate: {
		id: 'app.settings.updates.up-to-date',
		defaultMessage: 'Axolotl is up to date.',
	},
	disabled: {
		id: 'app.settings.updates.disabled',
		defaultMessage: 'Updates are disabled in this build.',
	},
	offline: {
		id: 'app.settings.updates.offline',
		defaultMessage: 'Connect to the internet to check for updates.',
	},
	failed: {
		id: 'app.settings.updates.failed',
		defaultMessage: 'Could not check for updates.',
	},
	portable: {
		id: 'app.settings.updates.portable',
		defaultMessage:
			'Portable mode cannot update automatically. Please download the latest version manually.',
	},
	security: {
		id: 'app.settings.updates.security',
		defaultMessage: 'Updates are installed only when their cryptographic signature is valid.',
	},
	preview: {
		id: 'app.settings.updates.preview-announcement',
		defaultMessage: 'Preview update announcement',
	},
	restartTitle: {
		id: 'app.settings.updates.channel.restart-title',
		defaultMessage: 'Restart required',
	},
	restartDescription: {
		id: 'app.settings.updates.channel.restart-description',
		defaultMessage:
			'Restart Axolotl now to start using the new update channel, or restart manually later.',
	},
	restartDevelopmentDescription: {
		id: 'app.settings.updates.channel.restart-development-description',
		defaultMessage:
			'The new update channel will be used after you manually restart the development session.',
	},
	restartNow: {
		id: 'app.settings.updates.channel.restart-now',
		defaultMessage: 'Restart now',
	},
	restartLater: {
		id: 'app.settings.updates.channel.restart-later',
		defaultMessage: 'Restart manually later',
	},
	immediateFetch: {
		id: 'app.settings.updates.immediate-fetch',
		defaultMessage: 'Get updates as soon as they are available',
	},
	immediateFetchDescription: {
		id: 'app.settings.updates.immediate-fetch-description',
		defaultMessage:
			'Release updates wait 24 hours by default. Beta updates are always available immediately.',
	},
	pause: {
		id: 'app.settings.updates.pause',
		defaultMessage: 'Pause updates',
	},
	pauseDescription: {
		id: 'app.settings.updates.pause-description',
		defaultMessage:
			'Stop automatic update checks, downloads, and update notifications until you resume updates.',
	},
	paused: {
		id: 'app.settings.updates.paused',
		defaultMessage: 'Updates are paused.',
	},
	copyDatabaseTitle: {
		id: 'app.settings.updates.channel.copy-database-title',
		defaultMessage: 'Copy Release data to Beta?',
	},
	copyDatabaseDescription: {
		id: 'app.settings.updates.channel.copy-database-description',
		defaultMessage:
			'Would you like to copy your Release database into the Beta channel? This cannot be undone automatically.',
	},
	copyDatabase: {
		id: 'app.settings.updates.channel.copy-database',
		defaultMessage: 'Copy database',
	},
	startEmpty: {
		id: 'app.settings.updates.channel.start-empty',
		defaultMessage: 'Start with empty database',
	},
})

const options: Array<{ value: UpdateChannel; label: string }> = [
	{ value: 'release', label: formatMessage(messages.release) },
	{ value: 'beta', label: formatMessage(messages.beta) },
]

const resultMessages: Record<AppUpdateCheckResult | 'failed' | 'portable', keyof typeof messages> =
	{
		available: 'available',
		'up-to-date': 'upToDate',
		disabled: 'disabled',
		offline: 'offline',
		failed: 'failed',
		portable: 'portable',
		paused: 'paused',
	}

watch(selectedChannel, async (channel, previousChannel) => {
	if (restoringChannelSelection) return

	if (channel === 'beta') updatePreferences.value.immediateUpdateFetch = true
	if (channel === 'beta' && previousChannel === 'release') {
		try {
			if (!(await betaDatabaseExists())) {
				pendingChannel.value = channel
				restoringChannelSelection = true
				selectedChannel.value = previousChannel
				await nextTick()
				restoringChannelSelection = false
				copyDatabaseModal.value?.show()
				return
			}
		} catch (error) {
			restoringChannelSelection = true
			selectedChannel.value = previousChannel
			await nextTick()
			restoringChannelSelection = false
			handleError(error)
			return
		}
	}

	await applyChannel(channel)
	checkResult.value = null
})

async function applyChannel(channel: UpdateChannel, copyDatabase = false) {
	try {
		if (copyDatabase) await copyReleaseDatabaseToBeta()
		await setUpdateChannel(channel)
		restartModal.value?.show()
		return true
	} catch (error) {
		handleError(error)
		return false
	}
}

async function chooseBetaDatabase(copyDatabase: boolean) {
	copyDatabaseModal.value?.hide()
	const channel = pendingChannel.value
	pendingChannel.value = null
	if (channel && (await applyChannel(channel, copyDatabase))) {
		restoringChannelSelection = true
		selectedChannel.value = channel
		await nextTick()
		restoringChannelSelection = false
	}
}

async function restartForChannelChange() {
	restartModal.value?.hide()
	try {
		await restartApp()
	} catch (error) {
		handleError(error)
	}
}

async function saveUpdatePreferences() {
	try {
		await setUpdatePreferences(updatePreferences.value)
	} catch (error) {
		handleError(error)
	}
}

async function checkForUpdates() {
	checking.value = true
	checkResult.value = null

	if (isPortable.value) {
		checkResult.value = 'portable'
		checking.value = false
		return
	}

	try {
		checkResult.value = await checkForAppUpdate()
	} catch (error) {
		checkResult.value = 'failed'
		handleError(error)
	} finally {
		checking.value = false
	}
}
</script>

<template>
	<div class="flex flex-col gap-6">
		<SettingsSection>
			<SettingsRow>
				<template #label>
					<span id="settings-target-updates-channel" tabindex="-1">
						{{ formatMessage(messages.title) }}
					</span>
				</template>
				<template #description>{{ formatMessage(messages.description) }}</template>
				<template #control>
					<Combobox
						id="update-channel"
						v-model="selectedChannel"
						:name="formatMessage(messages.title)"
						:options="options"
					/>
				</template>
			</SettingsRow>
		</SettingsSection>

		<SettingsSection>
			<SettingsRow>
				<template #label>{{ formatMessage(messages.immediateFetch) }}</template>
				<template #description>{{ formatMessage(messages.immediateFetchDescription) }}</template>
				<template #control>
					<Toggle
						id="immediate-update-fetch"
						v-model="updatePreferences.immediateUpdateFetch"
						:disabled="selectedChannel === 'beta'"
						@update:model-value="saveUpdatePreferences"
					/>
				</template>
			</SettingsRow>
			<SettingsRow>
				<template #label>{{ formatMessage(messages.pause) }}</template>
				<template #description>{{ formatMessage(messages.pauseDescription) }}</template>
				<template #control>
					<Toggle
						id="pause-updates"
						v-model="updatePreferences.updatesPaused"
						@update:model-value="saveUpdatePreferences"
					/>
				</template>
			</SettingsRow>
		</SettingsSection>

		<SettingsSection>
			<div class="flex flex-col items-start gap-3 p-4">
				<div class="flex flex-wrap gap-2">
					<Button type="colored" color="brand" :disabled="checking" @click="checkForUpdates">
						<RefreshCwIcon :class="{ 'animate-spin': checking }" />
						{{ formatMessage(checking ? messages.checking : messages.check) }}
					</Button>
					<Button
						v-if="isDevEnvironment && previewUpdateAnnouncement"
						type="outlined"
						native-type="button"
						@click="previewUpdateAnnouncement(currentVersion)"
					>
						<EyeIcon />
						{{ formatMessage(messages.preview) }}
					</Button>
				</div>
				<p v-if="checkResult" class="m-0 text-sm text-secondary" role="status">
					{{ formatMessage(messages[resultMessages[checkResult]]) }}
				</p>
			</div>
		</SettingsSection>

		<p class="settings-note">{{ formatMessage(messages.security) }}</p>

		<UpdateAnnouncementHistory :current-version="currentVersion" />
	</div>

	<NewModal ref="restartModal" :header="formatMessage(messages.restartTitle)" :closable="false">
		<p class="m-0">
			{{
				formatMessage(
					isDevEnvironment ? messages.restartDevelopmentDescription : messages.restartDescription,
				)
			}}
		</p>
		<template #actions>
			<div class="flex justify-end gap-2">
				<ButtonStyled type="outlined">
					<button type="button" @click="restartModal?.hide()">
						<XIcon />
						{{ formatMessage(messages.restartLater) }}
					</button>
				</ButtonStyled>
				<ButtonStyled v-if="!isDevEnvironment" color="brand">
					<button type="button" @click="restartForChannelChange">
						<RefreshCwIcon />
						{{ formatMessage(messages.restartNow) }}
					</button>
				</ButtonStyled>
			</div>
		</template>
	</NewModal>

	<NewModal
		ref="copyDatabaseModal"
		:header="formatMessage(messages.copyDatabaseTitle)"
		:closable="false"
	>
		<p class="m-0">{{ formatMessage(messages.copyDatabaseDescription) }}</p>
		<template #actions>
			<div class="flex justify-end gap-2">
				<ButtonStyled type="outlined">
					<button type="button" @click="chooseBetaDatabase(false)">
						<XIcon />
						{{ formatMessage(messages.startEmpty) }}
					</button>
				</ButtonStyled>
				<ButtonStyled color="brand">
					<button type="button" @click="chooseBetaDatabase(true)">
						<RefreshCwIcon />
						{{ formatMessage(messages.copyDatabase) }}
					</button>
				</ButtonStyled>
			</div>
		</template>
	</NewModal>
</template>

<style scoped>
.settings-note {
	margin: 0;
	padding: var(--gap-md) var(--gap-lg);
	border: 1px solid
		var(--settings-card-border, color-mix(in srgb, var(--surface-4) 72%, transparent));
	border-radius: var(--radius-md);
	background: var(--surface-2);
	color: var(--color-secondary);
	font-size: 0.8125rem;
	line-height: 1.5;
}
</style>
