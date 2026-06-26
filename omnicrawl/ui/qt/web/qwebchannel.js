/****************************************************************************
**
** Copyright (C) 2016 The Qt Company Ltd.
** Contact: https://www.qt.io/licensing/
**
** This file is part of the examples of the Qt Toolkit.
**
** $QT_BEGIN_LICENSE:BSD$
** Commercial License Usage
** Licensees holding valid commercial Qt licenses may use this file in
** accordance with the commercial license agreement provided with the
** Software or, alternatively, in accordance with the terms contained in
** a written agreement between you and The Qt Company. For licensing terms
** and conditions see https://www.qt.io/terms-conditions. For further
** information use the contact form at https://www.qt.io/contact-us.
**
** BSD License Usage
** Alternatively, you may use this file under the terms of the BSD license
** as follows:
**
** "Redistribution and use in source and binary forms, with or without
** modification, are permitted provided that the following conditions are
** met:
**   * Redistributions of source code must retain the above copyright
**     notice, this list of conditions and the following disclaimer.
**   * Redistributions in binary form must reproduce the above copyright
**     notice, this list of conditions and the following disclaimer in
**     the documentation and/or other materials provided with the
**     distribution.
**   * Neither the name of The Qt Company Ltd nor the names of its
**     contributors may be used to endorse or promote products derived
**     from this software without specific prior written permission.
**
**
** THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS
** "AS IS" AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT
** LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR
** A PARTICULAR PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT
** OWNER OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL,
** SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT
** LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE,
** DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY
** THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT
** (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
** OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE."
**
** $QT_END_LICENSE$
**
****************************************************************************/

"use strict";

var QWebChannelMessageTypes = {
    signal: 1,
    propertyUpdate: 2,
    init: 3,
    idle: 4,
    debug: 5,
    invokeMethod: 6,
    connectToSignal: 7,
    disconnectFromSignal: 8,
    setProperty: 9,
    response: 10,
};

var QWebChannel = function(transport, initCallback)
{
    if (typeof transport !== "object" || typeof transport.send !== "function") {
        console.error("The QWebChannel expects a transport object with a send function and onmessage callback property." +
                      " Given is: transport=" + typeof(transport) + ", transport.send=" + typeof(transport.send));
        return;
    }

    var channel = this;
    this.transport = transport;

    this.send = function(data)
    {
        if (typeof(data) !== "string") {
            data = JSON.stringify(data);
        }
        channel.transport.send(data);
    }

    this.transport.onmessage = function(message)
    {
        var data = message;
        // PyQt5 QWebChannel transport wraps data in message.data
        if (typeof data === "object" && data !== null && "data" in data) {
            data = data.data;
        }
        if (typeof data === "string") {
            data = JSON.parse(data);
        }
        switch(data.type) {
            case QWebChannelMessageTypes.signal:
                channel.handleSignal(data);
                break;
            case QWebChannelMessageTypes.response:
                channel.handleResponse(data);
                break;
            case QWebChannelMessageTypes.propertyUpdate:
                channel.handlePropertyUpdate(data);
                break;
            default:
                console.warn("Unhandled QWebChannel message type:", data.type, data);
                break;
        }
    }

    this.execCallbacks = {};
    this.execId = 0;
    this.exec = function(data, callback)
    {
        if (!callback) {
            // if no callback is given, send directly
            channel.send(data);
            return;
        }
        if (channel.execId === Number.MAX_VALUE) {
            // wrap
            channel.execId = Number.MIN_VALUE;
        }
        if (data.hasOwnProperty("id")) {
            console.error("Cannot exec message with property id: " + JSON.stringify(data));
            return;
        }
        data.id = channel.execId++;
        channel.execCallbacks[data.id] = callback;
        channel.send(data);
    };

    this.objects = {};

    this.handleSignal = function(message)
    {
        var object = channel.objects[message.object];
        if (object) {
            object.signalEmitted(message.signal, message.args);
        } else {
            console.warn("Undefined signal handler for object " + message.object + " and signal " + message.signal);
        }
    }

    this.handleResponse = function(message)
    {
        if (message.id === undefined || message.id === null) {
            console.error("Invalid response message received: ", JSON.stringify(message));
            return;
        }
        if (channel.execCallbacks[message.id]) {
            channel.execCallbacks[message.id](message.data);
            delete channel.execCallbacks[message.id];
        }
    }

    this.handlePropertyUpdate = function(message)
    {
        for (var i in message.data) {
            var data = message.data[i];
            var object = channel.objects[data.object];
            if (object) {
                object.propertyUpdate(data.signals, data.properties);
            } else {
                console.warn("Unhandled property update for object " + data.object);
            }
        }
        channel.exec({type: QWebChannelMessageTypes.idle});
    }

    this.debug = function(message)
    {
        channel.send({type: QWebChannelMessageTypes.debug, data: message});
    };

    channel.exec({type: QWebChannelMessageTypes.init}, function(data) {
        for (var objectName in data) {
            var object = new QObject(objectName, data[objectName], channel);
        }
        // now unwrap properties, which might reference other registered objects
        for (var objectName in channel.objects) {
            channel.objects[objectName].unwrapProperties();
        }
        if (initCallback) {
            initCallback(channel);
        }
        channel.exec({type: QWebChannelMessageTypes.idle});
    });
};

function QObject(name, data, webChannel)
{
    this.__id__ = name;
    webChannel.objects[name] = this;

    // List of callbacks that get invoked upon signal emission
    this.__objectSignals__ = {};

    // Cache of all properties, updated when a notify signal is emitted
    this.__propertyCache__ = {};

    // List of properties that have been changed since the last
    // property update message from the C++ side.
    this.__propertyUpdatedSignals__ = {};

    var object = this;

    // ----------------------------------------------------------------------

    this.unwrapQObject = function(response)
    {
        if (response instanceof Array) {
            // support list of objects
            var ret = new Array(response.length);
            for (var i = 0; i < response.length; ++i) {
                ret[i] = object.unwrapQObject(response[i]);
            }
            return ret;
        }
        if (!response
            || !response["__QObject*__"]
            || response.id === undefined) {
            return response;
        }

        var objectId = response.id;
        if (webChannel.objects[objectId])
            return webChannel.objects[objectId];

        if (!response.data) {
            console.error("Cannot unwrap unknown QObject " + objectId + " without data.");
            return;
        }

        var qObject = new QObject(objectId, response.data, webChannel);
        qObject.destroyed.connect(function() {
            if (webChannel.objects[objectId] === qObject) {
                delete webChannel.objects[objectId];
            }
        });
        // here we are already initialized, and thus must directly unwrap the properties
        qObject.unwrapProperties();
        return qObject;
    }

    this.unwrapProperties = function()
    {
        for (var propertyIdx in object.__propertyCache__) {
            object.__propertyCache__[propertyIdx] = object.unwrapQObject(object.__propertyCache__[propertyIdx]);
        }
    }

    function addSignal(eventData, isPropertyNotifySignal)
    {
        var signalName = eventData;
        var signalIndex = eventData;
        if (eventData instanceof Array) {
            signalName = eventData[0];
            signalIndex = eventData[1];
        }

        object.__objectSignals__[signalIndex] = {
            isPropertyNotifySignal: isPropertyNotifySignal,
            listeners: []
        };

        if (typeof signalName === "string" && object[signalName] === undefined) {
            object[signalName] = {
                connect: function(callback) {
                    object.connectToSignal(signalIndex, callback);
                },
                disconnect: function(callback) {
                    object.disconnectFromSignal(signalIndex, callback);
                }
            };
        }
    }

    function invokeSignal(signalName, signalArgs)
    {
        var connections = object.__objectSignals__[signalName];
        if (connections) {
            for (var i = 0; i < connections.listeners.length; ++i) {
                connections.listeners[i](signalArgs);
            }
        }
    }

    this.signalEmitted = function(signalName, signalArgs)
    {
        invokeSignal(signalName, this.unwrapQObject(signalArgs));
    }

    function propertyUpdate(signalName, propertyIdx)
    {
        // update property cache
        object.__propertyCache__[propertyIdx] = object.unwrapQObject(signalArgs[propertyIdx]);

        invokeSignal(signalName, signalArgs);
    }

    this.propertyUpdate = function(signals, propertyData)
    {
        for (var signalIndex in signals) {
            var signalName = signals[signalIndex][0];
            var signalArgs = signals[signalIndex][1];
            invokeSignal(signalName, object.unwrapQObject(signalArgs));
            var propertyIdx = signals[signalIndex][2];
            if (propertyIdx !== undefined) {
                object.__propertyCache__[propertyIdx] = object.unwrapQObject(propertyData[propertyIdx]);
            }
        }
    }

    this.setProperty = function(propertyIdx, value)
    {
        object.__propertyCache__[propertyIdx] = value;
        webChannel.exec({
            "type": QWebChannelMessageTypes.setProperty,
            "object": object.__id__,
            "property": propertyIdx,
            "value": value
        });
    };

    // ----------------------------------------------------------------------

    this.invokeMethod = function(methodName, args, callback)
    {
        if (args === undefined)
            args = [];
        if (callback === undefined)
            callback = null;
        webChannel.exec({
            "type": QWebChannelMessageTypes.invokeMethod,
            "object": object.__id__,
            "method": methodName,
            "args": args
        }, function(response) {
            if (callback) {
                callback(object.unwrapQObject(response));
            }
        });
    };

    function addMethod(methodData)
    {
        var methodName = methodData[0];
        var methodIndex = methodData[1];

        object[methodName] = function() {
            var args = [];
            var callback;
            for (var i = 0; i < arguments.length; ++i) {
                if (typeof arguments[i] === "function") {
                    callback = arguments[i];
                } else {
                    args.push(arguments[i]);
                }
            }
            object.invokeMethod(methodIndex, args, callback);
        };

        var overloadStart = methodName.indexOf("(");
        if (overloadStart !== -1) {
            var shortName = methodName.substring(0, overloadStart);
            if (object[shortName] === undefined) {
                object[shortName] = object[methodName];
            }
        }
    }

    function addProperty(propertyData)
    {
        var propertyIndex = propertyData[0];
        var propertyName = propertyData[1];
        var notifySignalData = propertyData[2];
        var propertyValue = propertyData[3];

        object.__propertyCache__[propertyIndex] = propertyValue;

        if (notifySignalData) {
            addSignal(notifySignalData, true);
        }

        Object.defineProperty(object, propertyName, {
            configurable: true,
            get: function() {
                return object.__propertyCache__[propertyIndex];
            },
            set: function(val) {
                if (typeof val === "object" && val !== null && val.__id__ !== undefined) {
                    object.setProperty(propertyIndex, val.__id__);
                } else {
                    object.setProperty(propertyIndex, val);
                }
            }
        });
    }

    // ----------------------------------------------------------------------

    function signalConnectionReady(signalName, callback)
    {
        if (!object.__objectSignals__[signalName]) {
            console.warn("Cannot connect to signal " + signalName + " on object " + object.__id__);
            return false;
        }
        return true;
    }

    this.connectToSignal = function(signalName, callback)
    {
        if (!signalConnectionReady(signalName, callback))
            return;

        object.__objectSignals__[signalName].listeners.push(callback);
        if(!object.__objectSignals__[signalName].isPropertyNotifySignal) {
            webChannel.exec({
                "type": QWebChannelMessageTypes.connectToSignal,
                "object": object.__id__,
                "signal": signalName
            });
        }
    };

    this.disconnectFromSignal = function(signalName, callback)
    {
        if (object.__objectSignals__[signalName]) {
            var idx = object.__objectSignals__[signalName].listeners.indexOf(callback);
            if (idx !== -1) {
                object.__objectSignals__[signalName].listeners.splice(idx, 1);
            }
            if (object.__objectSignals__[signalName].listeners.length === 0 &&
                !object.__objectSignals__[signalName].isPropertyNotifySignal) {
                webChannel.exec({
                    "type": QWebChannelMessageTypes.disconnectFromSignal,
                    "object": object.__id__,
                    "signal": signalName
                });
            }
        }
    };

    // ----------------------------------------------------------------------

    this.signalEmitted = function(signalName, signalArgs)
    {
        invokeSignal(signalName, this.unwrapQObject(signalArgs));
    };

    // ----------------------------------------------------------------------

    this.property = function(name)
    {
        return object.__propertyCache__[name];
    };

    // ----------------------------------------------------------------------

    this.destroyed = function() {};
    addSignal("destroyed", false);

    // ----------------------------------------------------------------------

    if (data.methods instanceof Array || data.properties instanceof Array || data.signals instanceof Array) {
        if (data.methods instanceof Array) {
            data.methods.forEach(addMethod);
        }
        if (data.properties instanceof Array) {
            data.properties.forEach(addProperty);
        }
        if (data.signals instanceof Array) {
            data.signals.forEach(function(signalData) {
                addSignal(signalData, false);
            });
        }
        return;
    }

    for (var propName in data) {
        var propVal = data[propName];
        if (propName.indexOf("notify:") === 0) {
            // skip notify signals
            addSignal(propName, true);
            continue;
        }
        if (propName.indexOf("signal:") === 0) {
            addSignal(propName, false);
            continue;
        }

        // cache properties
        object.__propertyCache__[propName] = propVal;

        // add property accessors
        Object.defineProperty(object, propName, {
            configurable: true,
            get: function() {
                return object.__propertyCache__[propName];
            },
            set: function(val) {
                if (typeof val === "object" && val !== null && val.__id__ !== undefined) {
                    // a QObject is passed as a property value
                    object.setProperty(propName, val.__id__);
                } else {
                    object.setProperty(propName, val);
                }
            }
        });
    }
}

// Required for use as a module
if (typeof module !== 'undefined') {
    module.exports = QWebChannel;
}
