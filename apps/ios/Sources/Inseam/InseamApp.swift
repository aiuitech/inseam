import InseamKit
import SwiftUI

@main
struct InseamApp: App {
    // The model lives at app scope: it owns the one node handle, the
    // bridged hosts registered into it, and the call observer.
    @State private var model = NodeModel()

    var body: some Scene {
        WindowGroup {
            ContentView()
                .environment(model)
                .task { model.openNode() }
                // "Open in inseam" from the share sheet — an audio file or
                // a transcript — lands here; the attach sheet takes it.
                .onOpenURL { url in model.beginAttach(fileURL: url) }
        }
    }
}
