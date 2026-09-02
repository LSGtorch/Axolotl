import * as THREE from 'three'

export interface SolidSkinLayerDefinition {
	width: number
	height: number
	depth: number
	u: number
	v: number
}

type Face = 'down' | 'up' | 'north' | 'south' | 'west' | 'east'

const FACES: Face[] = ['down', 'up', 'north', 'south', 'west', 'east']

function opaque(pixels: Uint8ClampedArray, u: number, v: number): boolean {
	return u >= 0 && v >= 0 && u < 64 && v < 64 && pixels[(v * 64 + u) * 4 + 3] > 0
}

function addQuad(
	positions: number[],
	normals: number[],
	uvs: number[],
	corners: Array<[number, number, number]>,
	normal: [number, number, number],
	uvCorners: Array<[number, number]>,
): void {
	for (const index of [0, 1, 2, 0, 2, 3]) {
		positions.push(...corners[index])
		normals.push(...normal)
		uvs.push(...uvCorners[index])
	}
}

function addCube(
	positions: number[],
	normals: number[],
	uvs: number[],
	min: THREE.Vector3,
	max: THREE.Vector3,
	anchors: Partial<Record<Face, [number, number]>>,
	visible: Record<Face, boolean>,
): void {
	const fallbackAnchor =
		anchors.north ?? anchors.south ?? anchors.west ?? anchors.east ?? anchors.down ?? anchors.up
	if (!fallbackAnchor) return
	const faceUv = (face: Face, du: number, dv: number): [number, number] => {
		const [u, v] = anchors[face] ?? fallbackAnchor
		// GLTF's authored UVs inset every pixel edge by 1/4096. Without this
		// inset, exact boundaries can round into an adjacent arm pixel with
		// nearest filtering, which appears as a one-pixel UV shift.
		const inset = 1 / 4096
		return [
			(u + du + (du === 0 ? inset : -inset)) / 64,
			(v + dv + (dv === 0 ? inset : -inset)) / 64,
		]
	}

	if (visible.north)
		addQuad(
			positions,
			normals,
			uvs,
			[
				[max.x, min.y, min.z],
				[min.x, min.y, min.z],
				[min.x, max.y, min.z],
				[max.x, max.y, min.z],
			],
			[0, 0, -1],
			[faceUv('north', 1, 1), faceUv('north', 0, 1), faceUv('north', 0, 0), faceUv('north', 1, 0)],
		)
	if (visible.south)
		addQuad(
			positions,
			normals,
			uvs,
			[
				[min.x, min.y, max.z],
				[max.x, min.y, max.z],
				[max.x, max.y, max.z],
				[min.x, max.y, max.z],
			],
			[0, 0, 1],
			[faceUv('south', 0, 1), faceUv('south', 1, 1), faceUv('south', 1, 0), faceUv('south', 0, 0)],
		)
	if (visible.down)
		addQuad(
			positions,
			normals,
			uvs,
			[
				[min.x, min.y, min.z],
				[max.x, min.y, min.z],
				[max.x, min.y, max.z],
				[min.x, min.y, max.z],
			],
			[0, -1, 0],
			[faceUv('down', 1, 1), faceUv('down', 0, 1), faceUv('down', 0, 0), faceUv('down', 1, 0)],
		)
	if (visible.up)
		addQuad(
			positions,
			normals,
			uvs,
			[
				[min.x, max.y, max.z],
				[max.x, max.y, max.z],
				[max.x, max.y, min.z],
				[min.x, max.y, min.z],
			],
			[0, 1, 0],
			[faceUv('up', 1, 0), faceUv('up', 0, 0), faceUv('up', 0, 1), faceUv('up', 1, 1)],
		)
	if (visible.west)
		addQuad(
			positions,
			normals,
			uvs,
			[
				[min.x, min.y, min.z],
				[min.x, min.y, max.z],
				[min.x, max.y, max.z],
				[min.x, max.y, min.z],
			],
			[-1, 0, 0],
			[faceUv('west', 0, 1), faceUv('west', 1, 1), faceUv('west', 1, 0), faceUv('west', 0, 0)],
		)
	if (visible.east)
		addQuad(
			positions,
			normals,
			uvs,
			[
				[max.x, min.y, max.z],
				[max.x, min.y, min.z],
				[max.x, max.y, min.z],
				[max.x, max.y, max.z],
			],
			[1, 0, 0],
			[faceUv('east', 0, 1), faceUv('east', 1, 1), faceUv('east', 1, 0), faceUv('east', 0, 0)],
		)
}

function facePixel(face: Face, u: number, v: number, d: SolidSkinLayerDefinition) {
	if (face === 'down')
		return { x: u, y: d.height - 1, z: d.depth - 1 - v, tu: d.u + d.depth + u, tv: d.v + v }
	if (face === 'up')
		return {
			x: u,
			y: 0,
			z: d.depth - 1 - v,
			tu: d.u + d.depth + d.width + u,
			tv: d.v + v,
		}
	if (face === 'north')
		return {
			x: d.width - 1 - u,
			y: d.height - 1 - v,
			z: 0,
			tu: d.u + d.depth + u,
			tv: d.v + d.depth + v,
		}
	if (face === 'south')
		return {
			x: u,
			y: d.height - 1 - v,
			z: d.depth - 1,
			tu: d.u + d.depth + d.width + d.depth + u,
			tv: d.v + d.depth + v,
		}
	if (face === 'west')
		return {
			x: d.width - 1,
			y: d.height - 1 - v,
			z: d.depth - 1 - u,
			tu: d.u + u,
			tv: d.v + d.depth + v,
		}
	return {
		x: 0,
		y: d.height - 1 - v,
		z: u,
		tu: d.u + d.depth + d.width + u,
		tv: d.v + d.depth + v,
	}
}

export function createSolidSkinLayerGeometry(
	mesh: THREE.Mesh,
	_texture: THREE.Texture,
	pixels: Uint8ClampedArray,
	d: SolidSkinLayerDefinition,
): THREE.BufferGeometry | null {
	const position = mesh.geometry.getAttribute('position') as THREE.BufferAttribute | undefined
	if (!position) return null
	const bounds = new THREE.Box3().setFromBufferAttribute(position)
	const size = bounds.getSize(new THREE.Vector3())
	const voxel = new THREE.Vector3(size.x / d.width, size.y / d.height, size.z / d.depth)
	const voxels = new Map<
		string,
		{ x: number; y: number; z: number; anchors: Partial<Record<Face, [number, number]>> }
	>()

	for (const face of FACES) {
		const faceWidth = face === 'west' || face === 'east' ? d.depth : d.width
		const faceHeight = face === 'down' || face === 'up' ? d.depth : d.height
		for (let u = 0; u < faceWidth; u++) {
			for (let v = 0; v < faceHeight; v++) {
				const p = facePixel(face, u, v, d)
				if (!opaque(pixels, p.tu, p.tv)) continue
				const key = `${p.x},${p.y},${p.z}`
				const entry = voxels.get(key) ?? { x: p.x, y: p.y, z: p.z, anchors: {} }
				// Corners can be hit by more than one source face. Preserve each
				// source anchor so the corresponding visible cube face samples the
				// correct pixel instead of inheriting a one-pixel-shifted neighbour.
				// Java's ModelPart uses Y-down and labels WEST as the player's
				// right side; GLTF uses Y-up and the opposite X-side labels.
				const geometryFace =
					face === 'down'
						? 'up'
						: face === 'up'
							? 'down'
							: face === 'west'
								? 'east'
								: face === 'east'
									? 'west'
									: face
				entry.anchors[geometryFace] = [p.tu, p.tv]
				voxels.set(key, entry)
			}
		}
	}

	if (!voxels.size) return null
	const positions: number[] = []
	const normals: number[] = []
	const uvs: number[] = []
	const hasVoxel = (x: number, y: number, z: number) => voxels.has(`${x},${y},${z}`)

	// Keep neighbouring cubes microscopically overlapped. GLTF arm bounds use
	// fractional coordinates; a fixed 1e-4 gap is still visible after projection.
	// A small fraction of one voxel closes the seam without changing the layer
	// silhouette. The value is scaled per axis below, so slim arms get enough
	// coverage while the head remains visually unchanged.
	const epsilon = 0.01
	for (const voxelPosition of voxels.values()) {
		const min = new THREE.Vector3(
			bounds.min.x + voxelPosition.x * voxel.x - voxel.x * epsilon,
			bounds.min.y + voxelPosition.y * voxel.y - voxel.y * epsilon,
			bounds.min.z + voxelPosition.z * voxel.z - voxel.z * epsilon,
		)
		const max = min
			.clone()
			.add(voxel)
			.add(new THREE.Vector3(voxel.x * epsilon * 2, voxel.y * epsilon * 2, voxel.z * epsilon * 2))
		const { x, y, z } = voxelPosition
		addCube(positions, normals, uvs, min, max, voxelPosition.anchors, {
			down: !hasVoxel(x, y - 1, z),
			up: !hasVoxel(x, y + 1, z),
			north: !hasVoxel(x, y, z - 1),
			south: !hasVoxel(x, y, z + 1),
			west: !hasVoxel(x - 1, y, z),
			east: !hasVoxel(x + 1, y, z),
		})
	}

	if (!positions.length) return null
	const geometry = new THREE.BufferGeometry()
	geometry.setAttribute('position', new THREE.Float32BufferAttribute(positions, 3))
	geometry.setAttribute('normal', new THREE.Float32BufferAttribute(normals, 3))
	geometry.setAttribute('uv', new THREE.Float32BufferAttribute(uvs, 2))
	geometry.computeBoundingBox()
	geometry.computeBoundingSphere()
	return geometry
}
