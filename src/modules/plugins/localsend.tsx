// LocalSend module: send the selected photo(s)/version to a device on the LAN using
// LocalSend's documented HTTP v2 protocol (the `localsend` backend feature). It is a
// publish TARGET — one destination in the unified Publish dialog — but a transfer, not a
// publication: it records nothing. The shared SendToDevicePanel does all the work; the
// Snapchat module reuses the same panel with an onSent + a 9:16 preflight. See
// docs/localsend.md.

import type { ChairPhotoModule } from "../registry";
import { SendToDevicePanel } from "./SendToDevicePanel";

export const localsendModule: ChairPhotoModule = {
  id: "localsend",
  name: "LocalSend",
  version: "0.1.0",
  description:
    "Send a photo to a device on the local network (phone, laptop) via LocalSend's HTTP protocol. Transfer only — records no publication.",
  backendFeature: "localsend",
  onLoad(api) {
    api.registerPublishTarget({
      id: "localsend",
      label: "Device (LocalSend)",
      render: () => <SendToDevicePanel api={api} />,
    });
  },
};
