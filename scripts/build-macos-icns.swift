import Foundation

struct IconRepresentation {
    let type: String
    let fileName: String
    let pixelSize: UInt32
}

let representations = [
    IconRepresentation(type: "ic04", fileName: "icon_16x16.png", pixelSize: 16),
    IconRepresentation(type: "ic11", fileName: "icon_16x16@2x.png", pixelSize: 32),
    IconRepresentation(type: "ic05", fileName: "icon_32x32.png", pixelSize: 32),
    IconRepresentation(type: "ic12", fileName: "icon_32x32@2x.png", pixelSize: 64),
    IconRepresentation(type: "ic07", fileName: "icon_128x128.png", pixelSize: 128),
    IconRepresentation(type: "ic13", fileName: "icon_128x128@2x.png", pixelSize: 256),
    IconRepresentation(type: "ic08", fileName: "icon_256x256.png", pixelSize: 256),
    IconRepresentation(type: "ic14", fileName: "icon_256x256@2x.png", pixelSize: 512),
    IconRepresentation(type: "ic09", fileName: "icon_512x512.png", pixelSize: 512),
    IconRepresentation(type: "ic10", fileName: "icon_512x512@2x.png", pixelSize: 1024),
]

enum IconBuildError: Error, CustomStringConvertible {
    case invalidArguments
    case invalidPNG(String)
    case oversizedInput(String)
    case oversizedOutput

    var description: String {
        switch self {
        case .invalidArguments:
            return "usage: build-macos-icns <input.iconset> <output.icns>"
        case let .invalidPNG(message):
            return message
        case let .oversizedInput(path):
            return "PNG input is too large: \(path)"
        case .oversizedOutput:
            return "ICNS output exceeds the 32-bit container limit"
        }
    }
}

func readBigEndianUInt32(_ data: Data, at offset: Int) -> UInt32 {
    (UInt32(data[offset]) << 24)
        | (UInt32(data[offset + 1]) << 16)
        | (UInt32(data[offset + 2]) << 8)
        | UInt32(data[offset + 3])
}

func appendBigEndianUInt32(_ value: UInt32, to data: inout Data) {
    data.append(UInt8((value >> 24) & 0xff))
    data.append(UInt8((value >> 16) & 0xff))
    data.append(UInt8((value >> 8) & 0xff))
    data.append(UInt8(value & 0xff))
}

func validatedPNG(at url: URL, expectedSize: UInt32) throws -> Data {
    let data = try Data(contentsOf: url, options: .mappedIfSafe)
    let maximumPNGSize = 32 * 1024 * 1024
    guard data.count <= maximumPNGSize else {
        throw IconBuildError.oversizedInput(url.path)
    }

    let signature: [UInt8] = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]
    guard data.count >= 33,
          data.starts(with: signature),
          readBigEndianUInt32(data, at: 8) == 13,
          data[12] == 0x49,
          data[13] == 0x48,
          data[14] == 0x44,
          data[15] == 0x52
    else {
        throw IconBuildError.invalidPNG("invalid PNG header: \(url.path)")
    }

    let width = readBigEndianUInt32(data, at: 16)
    let height = readBigEndianUInt32(data, at: 20)
    guard width == expectedSize, height == expectedSize else {
        throw IconBuildError.invalidPNG(
            "unexpected PNG dimensions for \(url.path): \(width)x\(height), expected \(expectedSize)x\(expectedSize)"
        )
    }
    guard data[24] == 8, data[25] == 6 else {
        throw IconBuildError.invalidPNG(
            "PNG must use 8-bit RGBA pixels: \(url.path)"
        )
    }
    return data
}

func buildIconset(inputDirectory: URL, output: URL) throws {
    var chunks = Data()
    for representation in representations {
        guard representation.type.utf8.count == 4 else {
            throw IconBuildError.invalidPNG("invalid ICNS representation type")
        }
        let input = inputDirectory.appendingPathComponent(representation.fileName)
        let png = try validatedPNG(at: input, expectedSize: representation.pixelSize)
        let chunkLength = 8 + png.count
        guard let encodedChunkLength = UInt32(exactly: chunkLength) else {
            throw IconBuildError.oversizedOutput
        }
        chunks.append(contentsOf: representation.type.utf8)
        appendBigEndianUInt32(encodedChunkLength, to: &chunks)
        chunks.append(png)
    }

    let totalLength = 8 + chunks.count
    guard let encodedTotalLength = UInt32(exactly: totalLength) else {
        throw IconBuildError.oversizedOutput
    }
    var outputData = Data("icns".utf8)
    appendBigEndianUInt32(encodedTotalLength, to: &outputData)
    outputData.append(chunks)
    try outputData.write(to: output, options: .atomic)
}

do {
    guard CommandLine.arguments.count == 3 else {
        throw IconBuildError.invalidArguments
    }
    let input = URL(fileURLWithPath: CommandLine.arguments[1], isDirectory: true)
    let output = URL(fileURLWithPath: CommandLine.arguments[2])
    try buildIconset(inputDirectory: input, output: output)
} catch {
    FileHandle.standardError.write(Data("macOS ICNS build failed: \(error)\n".utf8))
    exit(EXIT_FAILURE)
}
