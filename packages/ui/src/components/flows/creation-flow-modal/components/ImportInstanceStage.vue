<template>
	<div
		data-onboarding-id="creation-import"
		class="flex flex-col items-center gap-6 py-4"
	>
		<ButtonStyled color="brand" size="large" type="outlined">
			<button type="button" @click="handleOpenFilePicker">
				<FolderUpIcon />
				{{ formatMessage(messages.selectFile) }}
			</button>
		</ButtonStyled>

		<span class="text-center text-sm text-secondary">
			{{ formatMessage(messages.importPrompt) }}
		</span>
	</div>
</template>

<script setup lang="ts">
import { ButtonStyled, defineMessages, useVIntl } from '@modrinth/ui'
import { FolderUpIcon } from '@modrinth/assets'

import { injectCreationFlowContext } from '../creation-flow-context'

const ctx = injectCreationFlowContext()
const { formatMessage } = useVIntl()

const messages = defineMessages({
	selectFile: {
		id: 'creation-flow.modal.import-instance.select-file',
		defaultMessage: 'Select file or folder to import',
	},
	importPrompt: {
		id: 'creation-flow.modal.import-instance.import-prompt',
		defaultMessage:
			'Drag & drop launcher folders, modpack files, or .minecraft folders to import an instance in one click',
	},
})

// ── Native file picker ──

async function handleOpenFilePicker() {
	try {
		const { open } = await import('@tauri-apps/plugin-dialog')
		const result = await open({ multiple: false })
		const filePath = typeof result === 'string' ? result : (result?.path ?? null)
		if (!filePath) return

		if (ctx.onImportFileReceived) {
			ctx.onImportFileReceived({
				file: null,
				filePath,
				source: 'file-picker',
			})
			return
		}

		// Fallback: set path directly on context
		ctx.modpackFile.value = null
		ctx.modpackFilePath.value = filePath
		if (ctx.finishDisabled.value) return
		if (ctx.flowType === 'instance') {
			ctx.finish()
		} else {
			ctx.modal.value?.setStage('final-config')
		}
	} catch {
		// do nothing
	}
}
</script>
