<script setup lang="ts">
import { ChevronUpIcon } from '@modrinth/assets'
import { onBeforeUnmount, onMounted, ref } from 'vue'

import ButtonStyled from './ButtonStyled.vue'

const visible = ref(false)
const sidebarVisible = ref(false)
let scrollContainer: Element | null = null
let observer: MutationObserver | null = null

function update() {
	visible.value = (scrollContainer?.scrollTop ?? 0) > 300
}

function updateSidebar() {
	sidebarVisible.value = document.querySelector('.app-contents')?.classList.contains('sidebar-enabled') ?? false
}

function scrollToTop() {
	scrollContainer?.scrollTo({ top: 0, behavior: 'smooth' })
}

onMounted(() => {
	scrollContainer = document.querySelector('.app-viewport')
	if (scrollContainer) {
		scrollContainer.addEventListener('scroll', update, { passive: true })
		update()
	}
	updateSidebar()
	const appContents = document.querySelector('.app-contents')
	if (appContents) {
		observer = new MutationObserver(updateSidebar)
		observer.observe(appContents, { attributes: true, attributeFilter: ['class'] })
	}
})

onBeforeUnmount(() => {
	scrollContainer?.removeEventListener('scroll', update)
	observer?.disconnect()
})
</script>

<template>
	<Transition name="scroll-to-top">
		<div
			v-if="visible"
			class="scroll-to-top-wrapper"
			:class="{ 'sidebar-visible': sidebarVisible }"
		>
			<ButtonStyled circular size="large" color="brand">
				<button
					class="scroll-to-top-btn"
					type="button"
					aria-label="Scroll to top"
					v-tooltip="'Scroll to top'"
					@click="scrollToTop"
				>
					<ChevronUpIcon aria-hidden="true" />
				</button>
			</ButtonStyled>
		</div>
	</Transition>
</template>

<style scoped>
.scroll-to-top-btn {
	@apply shadow-lg transition-all duration-200 hover:brightness-110 hover:shadow-xl active:scale-95;
}

.scroll-to-top-wrapper {
	@apply fixed bottom-6 z-50;
	right: 24px;
}

.scroll-to-top-wrapper.sidebar-visible {
	right: calc(300px + 24px);
}

.scroll-to-top-enter-active,
.scroll-to-top-leave-active {
	transition: opacity 0.24s ease, transform 0.24s ease;
}

.scroll-to-top-enter-from,
.scroll-to-top-leave-to {
	opacity: 0;
	transform: translateY(10px);
}
</style>
